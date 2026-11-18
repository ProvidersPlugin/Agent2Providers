//! 原生信封中的内容归并/工具往返规则。签名保持原部件，不合并带签名的 Part。

use super::{image_input, signatures};
use agent2api_wasm_sdk::{json, Error, Result, Value};
use std::collections::BTreeMap;

pub fn string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
pub fn field(value: &Value, name: &str) -> String {
    value.get(name).map(string).unwrap_or_default()
}
pub fn text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => {
            let mut out = String::new();
            for part in parts {
                if let Some(text) = part.as_str() {
                    out.push_str(text);
                    continue;
                }
                let kind = field(part, "type").to_ascii_lowercase();
                if matches!(kind.as_str(), "image" | "image_url" | "input_image") {
                    continue;
                }
                let text = field(part, "text");
                if text.is_empty()
                    && !matches!(kind.as_str(), "text" | "input_text" | "output_text")
                {
                    out.push_str(&part.to_string());
                } else {
                    out.push_str(&text);
                }
            }
            out
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
pub fn role(message: &Value) -> String {
    let role = field(message, "role").trim().to_ascii_lowercase();
    if role.is_empty() {
        "user".into()
    } else {
        role
    }
}
fn push_turn(contents: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if let Some(last) = contents.last_mut().filter(|turn| turn["role"] == role) {
        if let Some(old) = last["parts"].as_array_mut() {
            old.extend(parts);
            return;
        }
    }
    let mut turn = json!({ "role": role });
    turn["parts"] = Value::Array(parts);
    contents.push(turn);
}

fn parts(content: &Value, role: &str) -> Result<Vec<Value>> {
    if let Some(text) = content.as_str() {
        return Ok(if text.is_empty() {
            Vec::new()
        } else {
            vec![json!({ "text": text })]
        });
    }
    if !content.is_null() && !content.is_array() {
        return Err(Error::http(400, "Antigravity 消息正文必须是文本或内容数组"));
    }
    let mut output = Vec::new();
    for part in content.as_array().into_iter().flatten() {
        if let Some(text) = part.as_str().filter(|s| !s.is_empty()) {
            output.push(json!({ "text": text }));
            continue;
        }
        match field(part, "type").to_ascii_lowercase().as_str() {
            "text" | "input_text" | "output_text" => {
                let text = field(part, "text");
                if !text.is_empty() {
                    let mut value = json!({ "text": text });
                    if let Some(sig) = signatures::call_signature(part) {
                        value["thoughtSignature"] = json!(sig);
                        if part["thought"] == true {
                            value["thought"] = json!(true);
                        }
                    }
                    output.push(value);
                }
            }
            "image_url" | "input_image" => output.push(image_input::from_part(part)?),
            "image" => {
                if part["source"]["type"] != "base64" {
                    return Err(Error::http(400, "Antigravity 图片块缺少受支持的内联内容"));
                }
                let source = format!(
                    "data:{};base64,{}",
                    field(&part["source"], "media_type"),
                    field(&part["source"], "data")
                );
                output.push(image_input::data_uri(&source)?);
            }
            "thinking" if role == "assistant" => {
                // 无签名思考仅可由同账号原响应缓存恢复，不能降级成普通正文改变签名上下文。
                if let Some(sig) = part["signature"].as_str().filter(|s| !s.trim().is_empty()) {
                    output.push(json!({ "text": field(part, "thinking"), "thought": true, "thoughtSignature": sig }));
                }
            }
            "redacted_thinking" if role == "assistant" => {
                return Err(Error::http(
                    400,
                    "Antigravity 不能无损发送此已隐藏思考块，请使用保留原签名的历史",
                ));
            }
            _ if part["thought"] == true && signatures::call_signature(part).is_some() => {
                output.push(json!({ "text": field(part, "text"), "thought": true, "thoughtSignature": signatures::call_signature(part) }));
            }
            _ => {
                return Err(Error::http(
                    400,
                    "Antigravity 消息包含尚未适配的内容块，未静默丢弃",
                ))
            }
        }
    }
    Ok(output)
}

fn function(call: &Value) -> Result<Value> {
    if !call.is_object() || call["type"].as_str().is_some_and(|kind| kind != "function") {
        return Err(Error::http(
            400,
            "Antigravity 仅支持函数工具调用，未丢弃历史工具",
        ));
    }
    let name = field(&call["function"], "name").trim().to_string();
    if name.is_empty() {
        return Err(Error::http(400, "Antigravity 历史工具调用缺少名称"));
    }
    let args = &call["function"]["arguments"];
    let args = match args {
        Value::Object(_) => args.clone(),
        Value::String(text) if text.trim().is_empty() => json!({}),
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| {
                Error::http(
                    400,
                    "Antigravity 历史工具参数不是有效 JSON 对象，未替换为空参数",
                )
            })?,
        Value::Null => json!({}),
        _ => return Err(Error::http(400, "Antigravity 历史工具参数必须是对象")),
    };
    let mut value = json!({ "functionCall": { "name": name, "args": args } });
    let id = field(call, "id").trim().to_string();
    if !id.is_empty() {
        value["functionCall"]["id"] = json!(id);
    }
    if let Some(signature) = signatures::call_signature(call) {
        value["thoughtSignature"] = json!(signature);
    }
    Ok(value)
}
fn tool_result(text: &str) -> Value {
    match serde_json::from_str::<Value>(text) {
        Ok(value @ (Value::Object(_) | Value::Array(_))) => value,
        Ok(value) if !value.is_null() => json!({ "result": value }),
        _ => json!({ "result": text }),
    }
}

pub fn contents(messages: &[Value]) -> Result<Vec<Value>> {
    let mut names = BTreeMap::new();
    for message in messages
        .iter()
        .filter(|message| role(message) == "assistant")
    {
        if message
            .get("tool_calls")
            .is_some_and(|value| !value.is_null() && !value.is_array())
        {
            return Err(Error::http(400, "Antigravity 历史工具调用必须是数组"));
        }
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            let parsed = function(call)?;
            let id = parsed["functionCall"]["id"].as_str().unwrap_or("");
            let name = parsed["functionCall"]["name"].as_str().unwrap_or("");
            if !id.is_empty() && names.insert(id.to_string(), name.to_string()).is_some() {
                return Err(Error::http(
                    400,
                    "Antigravity 历史工具标识重复，不能确认结果归属",
                ));
            }
        }
    }
    let mut received = std::collections::BTreeSet::new();
    let mut contents = Vec::new();
    for message in messages {
        if !message.is_object()
            || message
                .get("tool_calls")
                .is_some_and(|value| !value.is_null() && !value.is_array())
        {
            return Err(Error::http(400, "Antigravity 消息或历史工具格式无效"));
        }
        match role(message).as_str() {
            "system" | "developer" => {}
            "tool" => {
                let id = field(message, "tool_call_id").trim().to_string();
                let provided = field(message, "name").trim().to_string();
                let name = if !id.is_empty() {
                    if !received.insert(id.clone()) {
                        return Err(Error::http(400, "Antigravity 同一工具结果重复回传"));
                    }
                    let name = names.get(&id).ok_or_else(|| {
                        Error::http(400, "Antigravity 工具结果没有对应的历史调用")
                    })?;
                    if !provided.is_empty() && &provided != name {
                        return Err(Error::http(400, "Antigravity 工具结果名称与原调用不同"));
                    }
                    name.clone()
                } else {
                    if provided.is_empty() {
                        return Err(Error::http(400, "Antigravity 工具结果缺少调用标识或名称"));
                    }
                    provided
                };
                let mut value = json!({ "functionResponse": { "name": name, "response": { "result": tool_result(&text(&message["content"])) } } });
                if !id.is_empty() {
                    value["functionResponse"]["id"] = json!(id);
                }
                push_turn(&mut contents, "user", vec![value]);
            }
            "assistant" => {
                let mut parts = parts(&message["content"], "assistant")?;
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    parts.push(function(call)?);
                }
                if !parts.is_empty() {
                    push_turn(&mut contents, "model", parts);
                }
            }
            _ => {
                let parts = parts(&message["content"], "user")?;
                if !parts.is_empty() {
                    push_turn(&mut contents, "user", parts);
                }
            }
        }
    }
    if contents.is_empty() {
        return Err(Error::http(400, "Antigravity messages 里没有可发送内容"));
    }
    if contents.last().is_some_and(|turn| turn["role"] != "user") {
        contents.push(
            json!({ "role": "user", "parts": [{ "text": "Please continue your analysis." }] }),
        );
    }
    if contents.first().is_some_and(|turn| turn["role"] != "user") {
        contents.insert(0, json!({ "role": "user", "parts": [{ "text": "..." }] }));
    }
    Ok(contents)
}

pub fn claude_leading(contents: &mut [Value]) {
    for turn in contents.iter_mut().filter(|turn| turn["role"] == "model") {
        let Some(parts) = turn["parts"].as_array_mut() else {
            continue;
        };
        // Claude 不接受 Gemini 的合成哨兵思考块；真实思考只在回填原完整部件时恢复。
        parts.retain(|part| {
            !(part["thought"] == true
                && signatures::call_signature(part)
                    .is_some_and(|signature| signatures::synthetic(&signature)))
        });
        for part in parts {
            if signatures::call_signature(part)
                .is_some_and(|signature| signatures::synthetic(&signature))
            {
                if let Some(fields) = part.as_object_mut() {
                    fields.remove("thoughtSignature");
                }
            }
        }
    }
}
