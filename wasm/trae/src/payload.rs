//! Trae 聊天请求体：白名单信封与原生工具消息整形。
//! 原生本家薄转换迁至WASM；随机改SDK，不复制宿主通用协议转换树。
//!
//! function、物理 model_name、输出上限和思考档位使用目录选中的同一条路由。
//! tools 的 parameters 必须序列化为字符串，历史 tool_calls 使用 function_call。
//! 历史 `-solo` / `-solo-max` 只作为兼容名解析，不能据此选择聊天通道或推断 Max 权益。

use serde_json::{json, Value};

use super::catalog::Route;
use agent2api_wasm_sdk::Error;

const SAMPLING_KEYS: [&str; 6] = [
    "temperature",
    "top_p",
    "presence_penalty",
    "frequency_penalty",
    "seed",
    "n",
];

pub fn prepare_body(mut obj: Value, route: &Route, session: &str) -> Result<Value, Error> {
    if !obj.is_object() {
        return Err(Error::http(400, "Trae 请求体必须是 JSON 对象"));
    }
    if obj
        .get("messages")
        .and_then(Value::as_array)
        .is_none_or(|rows| rows.iter().any(|row| !row.is_object()))
    {
        return Err(Error::http(400, "Trae messages 必须是对象数组"));
    }
    normalize_legacy_functions(&mut obj)?;
    normalize_tool_choice(&mut obj);
    normalize_tools(&mut obj)?;
    normalize_messages(&mut obj)?;
    drop_orphan_tool_results(&mut obj)?;
    normalize_tool_results(&mut obj);
    normalize_images(&mut obj, route.supports_images)?;
    if obj
        .get("messages")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
    {
        return Err(Error::http(400, "Trae 请求必须包含非空 messages 数组"));
    }

    let mut out = serde_json::Map::new();
    out.insert("function".to_string(), json!(route.function));
    out.insert("stream".to_string(), json!(true));
    out.insert("config_name".to_string(), json!(route.config_name));
    out.insert("model".to_string(), json!(route.config_name));
    if let Some(model_name) = &route.model_name {
        out.insert("model_name".to_string(), json!(model_name));
    }
    out.insert("session_id".to_string(), json!(session));
    out.insert("request_id".to_string(), json!(super::util::uuid()?));
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
    if let Some(asked) = effort.filter(|value| !value.is_empty()) {
        let effort = route.reasoning_effort(asked).ok_or_else(|| {
            Error::http(400, "Trae 当前账号目录未声明所选思考档位")
                .with_code("trae_route_capability")
        })?;
        out.insert("reasoning_effort".to_string(), json!(effort));
    }
    if let Some(fields) = obj.as_object_mut() {
        if let Some(messages) = fields.remove("messages") {
            out.insert("messages".to_string(), messages);
        }
        if let Some(tools) = fields
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty())
            .then(|| fields.remove("tools"))
            .flatten()
        {
            out.insert("tools".to_string(), tools);
            if let Some(choice) = fields.remove("tool_choice") {
                out.insert("tool_choice".to_string(), choice);
            }
        }
        if let Some(parallel) = fields
            .get("parallel_tool_calls")
            .filter(|value| value.is_boolean())
        {
            out.insert("parallel_tool_calls".to_string(), parallel.clone());
        }
        if let Some(stop @ (Value::String(_) | Value::Array(_))) = fields.remove("stop") {
            out.insert("stop".to_string(), stop);
        }
    }
    Ok(Value::Object(out))
}

fn normalize_legacy_functions(obj: &mut Value) -> Result<(), Error> {
    if obj.get("tools").is_none() {
        if let Some(functions) = obj.get("functions") {
            let list = functions
                .as_array()
                .filter(|list| list.len() <= 512)
                .ok_or_else(|| Error::http(400, "Trae functions 必须是受限长度数组"))?;
            obj["tools"] = json!(list
                .iter()
                .map(|function| json!({ "type": "function", "function": function }))
                .collect::<Vec<_>>());
        }
    }
    if obj.get("tool_choice").is_none() {
        if let Some(choice) = obj.get("function_call").cloned() {
            obj["tool_choice"] = match choice {
                Value::Object(function) => json!({ "type": "function", "function": function }),
                value => value,
            };
        }
    }
    if let Some(messages) = obj.get_mut("messages").and_then(Value::as_array_mut) {
        let mut pending: Vec<(String, String)> = Vec::new();
        for message in messages {
            if message["role"] == "assistant" {
                if message.get("tool_calls").is_none() {
                    if let Some(function) = message
                        .get("function_call")
                        .filter(|value| value.is_object())
                        .cloned()
                    {
                        let id = format!("call_{}", super::util::random_hex(12)?);
                        message["tool_calls"] =
                            json!([{ "id": id, "type": "function", "function": function }]);
                        message
                            .as_object_mut()
                            .map(|fields| fields.remove("function_call"));
                    }
                }
                pending = message["tool_calls"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|call| {
                        let id = call["id"].as_str()?.to_string();
                        let name = call
                            .pointer("/function/name")
                            .or_else(|| call.pointer("/function_call/name"))?
                            .as_str()?
                            .to_string();
                        Some((id, name))
                    })
                    .collect();
            } else if message["role"] == "function" {
                let name = message["name"].as_str().unwrap_or("");
                let position = pending
                    .iter()
                    .position(|(_, expected)| expected == name)
                    .or_else(|| (name.is_empty() && pending.len() == 1).then_some(0))
                    .ok_or_else(|| Error::http(400, "Trae 旧函数结果没有对应调用，未丢弃结果"))?;
                let (id, _) = pending.remove(position);
                message["role"] = json!("tool");
                message["tool_call_id"] = json!(id);
            }
        }
    }
    Ok(())
}

fn normalize_images(obj: &mut Value, supported: Option<bool>) -> Result<(), Error> {
    for message in obj["messages"].as_array_mut().into_iter().flatten() {
        for part in message["content"].as_array_mut().into_iter().flatten() {
            if part["type"] == "input_text" {
                part["type"] = json!("text");
            }
            if !matches!(part["type"].as_str(), Some("image_url" | "input_image")) {
                continue;
            }
            if supported == Some(false) {
                return Err(Error::http(
                    400,
                    "Trae 当前账号目录明确此模型不支持图片，请选择支持图片的模型",
                )
                .with_code("trae_route_capability"));
            }
            let url = part
                .get("image_url")
                .and_then(Value::as_str)
                .or_else(|| part.pointer("/image_url/url").and_then(Value::as_str))
                .or_else(|| part.get("url").and_then(Value::as_str))
                .ok_or_else(|| Error::http(400, "Trae 图片缺少有效地址"))?
                .to_string();
            if url.is_empty() || url.len() > 16 * 1024 * 1024 || url.chars().any(char::is_control) {
                return Err(Error::http(400, "Trae 图片地址无效或超过限制"));
            }
            let detail = part.pointer("/image_url/detail").cloned();
            *part = json!({ "type": "image_url", "image_url": { "url": url } });
            if let Some(detail) = detail {
                part["image_url"]["detail"] = detail;
            }
        }
    }
    Ok(())
}

/// ① 角色归一 + ② assistant tool_calls 改名 + ③ 字符串 content 归一成 text 块。
fn normalize_messages(obj: &mut Value) -> Result<(), Error> {
    let Some(messages) = obj
        .get_mut("messages")
        .and_then(|value| value.as_array_mut())
    else {
        return Ok(());
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
                            return Err(Error::http(400, "Trae 历史工具调用不是有效对象"));
                        };
                        if let Some(mut function) = call
                            .get("function")
                            .or_else(|| call.get("function_call"))
                            .cloned()
                        {
                            if function.is_object() {
                                if let Some(arguments) = function
                                    .get("arguments")
                                    .filter(|value| !value.is_string() && !value.is_null())
                                    .cloned()
                                {
                                    function["arguments"] = json!(arguments.to_string());
                                }
                            }
                            call.insert("function_call".to_string(), function);
                            call.remove("function");
                        }
                        // 上游要求 function_call.name 必填：没名字的调用整条剔除。
                        let named = call
                            .get("function_call")
                            .and_then(|fc| fc.get("name"))
                            .and_then(Value::as_str)
                            .is_some_and(|name| !name.trim().is_empty());
                        if !named {
                            return Err(Error::http(400, "Trae 历史工具调用缺少名称，未丢弃历史"));
                        }
                        if call
                            .get("id")
                            .and_then(Value::as_str)
                            .is_none_or(str::is_empty)
                        {
                            return Err(Error::http(400, "Trae 历史工具调用缺少ID，未丢弃历史"));
                        }
                        kept.push(Value::Object(call));
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
            // 这里刻意不用 `continue`：它在嵌套块里会跳过本回合块之后的
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
    Ok(())
}

/// 验证工具结果对应历史真实调用；不通过删除合法结果掩盖转换遗漏。
fn drop_orphan_tool_results(obj: &mut Value) -> Result<(), Error> {
    let Some(messages) = obj.get("messages").and_then(Value::as_array).cloned() else {
        return Ok(());
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
    for message in messages.iter().filter(|message| message["role"] == "tool") {
        let id = message["tool_call_id"].as_str().unwrap_or("");
        if id.is_empty() || !known.iter().any(|known| known == id) {
            return Err(Error::http(
                400,
                "Trae 工具结果没有对应历史调用，未丢弃结果",
            ));
        }
    }
    obj["messages"] = Value::Array(
        messages
            .into_iter()
            .filter(|message| !message.is_null())
            .collect(),
    );
    Ok(())
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
                for field in ["reasoning_content", "reasoning"] {
                    if let Some(text) = message.get(field).and_then(Value::as_str) {
                        let previous = last.get(field).and_then(Value::as_str).unwrap_or("");
                        last[field] = json!(format!("{previous}{text}"));
                    }
                }
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
fn normalize_tools(obj: &mut Value) -> Result<(), Error> {
    let Some(raw) = obj.get("tools").cloned() else {
        return Ok(());
    };
    let Value::Array(list) = raw else {
        return Err(Error::http(400, "Trae 工具声明必须是数组"));
    };
    if list.len() > 512 {
        return Err(Error::http(400, "Trae 工具声明数量超过限制"));
    }
    if list.is_empty() {
        return Ok(());
    }
    let mut kept: Vec<Value> = Vec::with_capacity(list.len());
    for item in list {
        let Value::Object(mut tool) = item else {
            return Err(Error::http(400, "Trae 工具声明不是对象"));
        };
        if tool.get("type").is_some_and(|value| value != "function") {
            return Err(Error::http(400, "Trae 当前只接受函数工具声明"));
        }
        let Some(Value::Object(mut function)) = tool.get("function").cloned() else {
            return Err(Error::http(400, "Trae 工具缺少函数声明"));
        };
        if !function
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| !name.trim().is_empty())
        {
            return Err(Error::http(400, "Trae 工具声明缺少有效名称"));
        }
        match function.get("parameters") {
            Some(Value::Object(_)) => {
                function.insert(
                    "parameters".to_string(),
                    json!(function["parameters"].to_string()),
                );
            }
            Some(Value::String(text)) => {
                if !serde_json::from_str::<Value>(text).is_ok_and(|value| value.is_object()) {
                    return Err(Error::http(400, "Trae 工具参数必须是有效 JSON 对象"));
                }
            }
            None | Some(Value::Null) => {
                function.insert(
                    "parameters".to_string(),
                    json!(json!({ "type": "object", "properties": {} }).to_string()),
                );
            }
            _ => return Err(Error::http(400, "Trae 工具参数声明不是有效对象")),
        }
        tool.insert("type".to_string(), json!("function"));
        tool.insert("function".to_string(), Value::Object(function));
        kept.push(Value::Object(tool));
    }
    if kept.is_empty() {
        if let Value::Object(map) = obj {
            map.remove("tools");
        }
        return Ok(());
    }
    obj["tools"] = Value::Array(kept);
    Ok(())
}
