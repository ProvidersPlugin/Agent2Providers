//! Trae 聊天请求体：白名单信封与原生工具消息整形。
//!
//! function、物理 model_name、输出上限和思考档位使用目录选中的同一条路由。
//! tools 的 parameters 必须序列化为字符串，历史 tool_calls 使用 function_call。
//! 历史 `-solo` / `-solo-max` 只作为兼容名解析，不能据此选择聊天通道或推断 Max 权益。

use serde_json::{json, Value};

use super::catalog::Route;
use super::models::legacy_target;
use crate::errors::GatewayError;

pub const DEFAULT_CONFIG_NAME: &str = "glm-5.2";

const SAMPLING_KEYS: [&str; 6] = [
    "temperature",
    "top_p",
    "presence_penalty",
    "frequency_penalty",
    "seed",
    "n",
];

/// 无目录路由时只使用既有兼容通道，不为未知模型猜物理模型名。
pub const PRIMARY_FUNCTION: &str = "solo_work_lite";

/// 只在目录查不到请求名时用于兜底：剥掉历史 `<id>-solo` / `<id>-solo-max` 后缀，
/// 不拆含 / 的上游 config，也不剥裸模型的 `-max`（那是上游真名的一部分）。
pub fn sanitize_model_name(model: &str) -> String {
    let name = model.trim();
    legacy_target(name)
        .map(|(base, _)| base.to_string())
        .unwrap_or_else(|| name.to_string())
}

pub fn is_max_variant(model: &str) -> bool {
    legacy_target(model.trim()).is_some_and(|(_, is_max)| is_max)
}

pub fn prepare_body(source: &Value, route: &Route, session: &str) -> Result<Value, GatewayError> {
    let Value::Object(body) = source else {
        return Err(GatewayError::with_status(
            400,
            "Trae 请求体必须是 JSON 对象",
        ));
    };
    let mut obj: Value = json!(body.clone());
    normalize_tool_choice(&mut obj);
    normalize_tools(&mut obj);
    normalize_messages(&mut obj);
    drop_orphan_tool_results(&mut obj);
    normalize_tool_results(&mut obj);
    let Some(messages) = obj
        .get("messages")
        .and_then(Value::as_array)
        .filter(|messages| !messages.is_empty())
    else {
        return Err(GatewayError::with_status(
            400,
            "Trae 请求必须包含非空 messages 数组",
        ));
    };

    let mut out = serde_json::Map::new();
    out.insert("messages".to_string(), json!(messages));
    out.insert("function".to_string(), json!(route.function));
    out.insert("stream".to_string(), json!(true));
    out.insert("config_name".to_string(), json!(route.config_name));
    out.insert("model".to_string(), json!(route.config_name));
    if let Some(model_name) = &route.model_name {
        out.insert("model_name".to_string(), json!(model_name));
    }
    out.insert("session_id".to_string(), json!(session));
    out.insert(
        "request_id".to_string(),
        json!(super::oauth::uuid_v4().map_err(GatewayError::new)?),
    );
    if let Some(tools) = obj
        .get("tools")
        .filter(|tools| tools.as_array().is_some_and(|tools| !tools.is_empty()))
    {
        out.insert("tools".to_string(), tools.clone());
        if let Some(choice) = obj.get("tool_choice") {
            out.insert("tool_choice".to_string(), choice.clone());
        }
        if let Some(parallel) = obj
            .get("parallel_tool_calls")
            .filter(|value| value.is_boolean())
        {
            out.insert("parallel_tool_calls".to_string(), parallel.clone());
        }
    }
    for key in SAMPLING_KEYS {
        if let Some(number) = obj.get(key).filter(|value| value.is_number()) {
            out.insert(key.to_string(), number.clone());
        }
    }
    let asked = ["max_completion_tokens", "max_tokens"]
        .into_iter()
        .filter_map(|key| obj.get(key).and_then(Value::as_i64))
        .find(|tokens| *tokens > 0);
    let output = match (asked, route.max_tokens) {
        (Some(asked), Some(native)) => Some(asked.min(native)),
        (Some(asked), None) => Some(asked),
        (None, native) if route.max_window.is_some() => native,
        _ => None,
    };
    if let Some(output) = output {
        out.insert("max_tokens".to_string(), json!(output));
    }
    if let Some(window) = route.max_window {
        out.insert(
            "user_message_context".to_string(),
            json!({
                "model_info": { "prompt_max_tokens": window.saturating_sub(output.unwrap_or(0)) }
            }),
        );
    }
    let effort = obj
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .or_else(|| {
            obj.get("reasoning")
                .and_then(|value| value.get("effort"))
                .and_then(Value::as_str)
        });
    if let Some(effort) = effort.and_then(|effort| route.reasoning_effort(effort)) {
        out.insert("reasoning_effort".to_string(), json!(effort));
    }
    match obj.get("stop") {
        Some(Value::String(_)) | Some(Value::Array(_)) => {
            out.insert("stop".to_string(), obj["stop"].clone());
        }
        _ => {}
    }
    Ok(Value::Object(out))
}

/// ① 角色归一 + ② assistant tool_calls 改名 + ③ 字符串 content 归一成 text 块。
fn normalize_messages(obj: &mut Value) {
    let Some(messages) = obj
        .get_mut("messages")
        .and_then(|value| value.as_array_mut())
    else {
        return;
    };
    for slot in messages.iter_mut() {
        // 占位标记要在借用结束后再写回（`*slot = Null` 与 `entry` 不能同时成立）。
        let mut mark_null = false;
        {
            let Value::Object(entry) = slot else {
                continue;
            };
            let content_present = entry.contains_key("content");
            let content_value = entry.get("content").cloned();
            let mut role = entry
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if role == "developer" {
                entry.insert("role".to_string(), json!("system"));
                role = "system".to_string();
            }

            if role == "assistant" {
                let calls = entry.get("tool_calls").and_then(Value::as_array).cloned();
                if let Some(calls) = calls {
                    let mut kept: Vec<Value> = Vec::with_capacity(calls.len());
                    for call in calls {
                        let Value::Object(mut call) = call else {
                            continue;
                        };
                        if let Some(function) = call.get("function").cloned() {
                            call.insert("function_call".to_string(), function);
                            call.remove("function");
                        }
                        // 上游要求 function_call.name 必填：没名字的调用整条剔除。
                        let named = call
                            .get("function_call")
                            .and_then(|fc| fc.get("name"))
                            .and_then(Value::as_str)
                            .is_some_and(|name| !name.trim().is_empty());
                        if named {
                            kept.push(Value::Object(call));
                        }
                    }
                    if kept.is_empty() {
                        entry.remove("tool_calls");
                        // 全被剔且本来就没正文的 assistant 占位消息整条丢掉：
                        // 上游对这种消息可能回空流。
                        let blank_content = content_value.as_ref().is_none_or(Value::is_null);
                        if !content_present || blank_content {
                            mark_null = true;
                        }
                    } else {
                        entry.insert("tool_calls".to_string(), Value::Array(kept));
                    }
                }
            }

            // 纯 tool_calls 的 assistant（没有 content 键）：字段留着，content 不改写。
            // ⚠️ 这里刻意不用 `continue`：它在嵌套块里会跳过本回合**块之后**的
            // `mark_null` 写回，占位消息就漏掉了（第一版就是这么错的）。
            // 已是数组（多模态）→ 原样透传：这里只匹配 String，非字符串落到
            // 什么都不做的那一支。参考实现注明"未实测，保守透传"，我们同样不猜：
            // 不改写就不会把客户端的图注弄坏。
            if let Some(Value::String(text)) = content_value {
                entry.insert(
                    "content".to_string(),
                    json!([{"type": "text", "text": text}]),
                );
            }
        }
        if mark_null {
            *slot = Value::Null;
        }
    }
}

/// 丢掉引用了不存在的 tool_call 的 `role=tool` 消息，以及上一步标记的占位。
fn drop_orphan_tool_results(obj: &mut Value) {
    let Some(messages) = obj.get("messages").and_then(Value::as_array).cloned() else {
        return;
    };
    let mut known: Vec<String> = Vec::new();
    for message in &messages {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(calls) = message.get("tool_calls").and_then(Value::as_array) else {
            continue;
        };
        for call in calls {
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                if !id.is_empty() {
                    known.push(id.to_string());
                }
            }
        }
    }
    let kept: Vec<Value> = messages
        .into_iter()
        .filter(|message| {
            if message.is_null() {
                return false; // normalize_messages 的占位标记
            }
            if message.get("role").and_then(Value::as_str) != Some("tool") {
                return true;
            }
            let id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            known.iter().any(|known_id| known_id == id)
        })
        .collect();
    obj["messages"] = Value::Array(kept);
}

/// Trae 不接受空工具结果；图片放在整组工具结果之后的 user 消息。
fn normalize_tool_results(obj: &mut Value) {
    let Some(messages) = obj.get("messages").and_then(Value::as_array).cloned() else {
        return;
    };
    let mut out: Vec<Value> = Vec::new();
    let mut pending_images = Vec::new();
    for mut message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if role == "tool" {
            let mut content = match message.get("content") {
                Some(Value::Array(parts)) => parts.clone(),
                Some(Value::String(text)) if !text.is_empty() => {
                    vec![json!({"type":"text", "text": text})]
                }
                _ => Vec::new(),
            };
            content.retain(|part| {
                if part.get("type").and_then(Value::as_str) == Some("image_url") {
                    pending_images.push(part.clone());
                    false
                } else {
                    true
                }
            });
            let has_text = content.iter().any(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.is_empty())
            });
            if !has_text {
                content.push(json!({ "type": "text", "text": if pending_images.is_empty() { "(no output)" } else { "(the result's image follows)" } }));
            }
            message["content"] = json!(content);
            out.push(message);
            continue;
        }
        if !pending_images.is_empty() {
            out.push(json!({ "role": "user", "content": std::mem::take(&mut pending_images) }));
        }
        if role == "assistant"
            && out
                .last()
                .is_some_and(|last| last.get("role").and_then(Value::as_str) == Some("assistant"))
        {
            if let Some(last) = out.last_mut() {
                for field in ["content", "tool_calls"] {
                    let mut parts = last
                        .get(field)
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    parts.extend(
                        message
                            .get(field)
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .cloned(),
                    );
                    if !parts.is_empty() {
                        last[field] = json!(parts);
                    }
                }
            }
        } else {
            out.push(message);
        }
    }
    if !pending_images.is_empty() {
        out.push(json!({ "role": "user", "content": pending_images }));
    }
    obj["messages"] = json!(out);
}

/// ④ `tool_choice` 折算成上游要的裸字符串；`"none"` 还要连带把 tools 删掉。
fn normalize_tool_choice(obj: &mut Value) {
    let Some(tool_choice) = obj.get("tool_choice").cloned() else {
        return;
    };
    let suppress_tools = |obj: &mut Value| {
        if let Value::Object(map) = obj {
            map.remove("tools");
            map.remove("functions");
        }
    };
    match tool_choice {
        Value::String(name) => {
            if name.trim().eq_ignore_ascii_case("none") {
                if let Value::Object(map) = obj {
                    map.remove("tool_choice");
                }
                suppress_tools(obj);
            }
        }
        Value::Object(ref wrapper) => {
            let kind = wrapper
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_lowercase();
            match kind.as_str() {
                "none" => {
                    if let Value::Object(map) = obj {
                        map.remove("tool_choice");
                    }
                    suppress_tools(obj);
                }
                "auto" | "required" => {
                    obj["tool_choice"] = json!(kind);
                }
                "function" => {
                    let name = wrapper
                        .get("function")
                        .and_then(|function| function.get("name"))
                        .and_then(Value::as_str)
                        .or_else(|| wrapper.get("name").and_then(Value::as_str))
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    obj["tool_choice"] = json!(if name.is_empty() {
                        "auto".to_string()
                    } else {
                        name
                    });
                }
                _ => {
                    if let Value::Object(map) = obj {
                        map.remove("tool_choice");
                    }
                }
            }
        }
        _ => {
            if let Value::Object(map) = obj {
                map.remove("tool_choice");
            }
        }
    }
}

/// ① `tools[].function.parameters` 对象 → JSON 字符串；不是这个形状的工具整条剔除。
fn normalize_tools(obj: &mut Value) {
    let Some(raw) = obj.get("tools").cloned() else {
        return;
    };
    let Value::Array(list) = raw else {
        return;
    };
    if list.is_empty() {
        return;
    }
    let mut kept: Vec<Value> = Vec::with_capacity(list.len());
    for item in list {
        let Value::Object(mut tool) = item else {
            continue;
        };
        let Some(Value::Object(mut function)) = tool.get("function").cloned() else {
            continue;
        };
        if !function
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| !name.trim().is_empty())
        {
            continue;
        }
        match function.get("parameters") {
            Some(Value::Object(_)) => {
                function.insert(
                    "parameters".to_string(),
                    json!(function["parameters"].to_string()),
                );
            }
            Some(Value::String(_)) => {}
            None | Some(Value::Null) => {
                function.insert(
                    "parameters".to_string(),
                    json!(json!({ "type": "object", "properties": {} }).to_string()),
                );
            }
            _ => continue,
        }
        tool.insert("function".to_string(), Value::Object(function));
        kept.push(Value::Object(tool));
    }
    if kept.is_empty() {
        if let Value::Object(map) = obj {
            map.remove("tools");
        }
        return;
    }
    obj["tools"] = Value::Array(kept);
}
