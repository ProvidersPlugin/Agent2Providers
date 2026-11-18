use super::{string, truthy};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn system_blocks(messages: &[Value]) -> Vec<Value> {
    let mut blocks = Vec::new();
    for message in messages {
        if !matches!(message["role"].as_str(), Some("system" | "developer")) {
            continue;
        }
        match message.get("content") {
            Some(Value::String(text)) if !text.is_empty() => {
                blocks.push(json!({ "type": "text", "text": text }))
            }
            Some(Value::Array(parts)) => {
                for part in parts {
                    let text = part
                        .get("text")
                        .filter(|value| !value.is_null())
                        .or_else(|| part.get("content").filter(|value| !value.is_null()))
                        .map(string)
                        .unwrap_or_default();
                    let cache = part.get("cache_control").filter(|value| truthy(value));
                    if text.is_empty() && cache.is_none() {
                        continue;
                    }
                    let mut block = json!({ "type": "text", "text": text });
                    if let Some(cache) = cache {
                        block["cache_control"] = cache.clone();
                    }
                    blocks.push(block);
                }
            }
            Some(Value::String(_)) => {}
            Some(value) if !value.is_null() => {
                blocks.push(json!({ "type": "text", "text": string(value) }))
            }
            _ => {}
        }
    }
    blocks
}

fn invalid(message: &str) -> Error {
    Error::http(400, message).with_code("invalid_request_error")
}

pub fn convert(messages: &[Value]) -> Result<Vec<Value>> {
    let mut names = std::collections::BTreeMap::new();
    let mut output: Vec<Value> = Vec::new();
    for message in messages {
        let role = message["role"]
            .as_str()
            .ok_or_else(|| invalid("Command Code 消息缺少角色"))?;
        let content = match role {
            "system" | "developer" => continue,
            "user" => user_content(message.get("content"))?,
            "assistant" => {
                let parts = assistant_content(message)?;
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    let id = call["id"].as_str().unwrap_or("");
                    let name = call["function"]["name"].as_str().unwrap_or("");
                    if names.insert(id.to_string(), name.to_string()).is_some() {
                        return Err(invalid("Command Code 历史中存在重复的工具调用标识"));
                    }
                }
                parts
            }
            "tool" => {
                let id = message["tool_call_id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| invalid("Command Code 工具结果缺少调用标识"))?;
                let name = names
                    .remove(id)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| invalid("Command Code 工具结果与前面的工具调用不匹配或重复"))?;
                vec![
                    json!({ "type": "tool-result", "toolCallId": id, "toolName": name,
                    "output": { "type": "text", "value": tool_output(message.get("content"))? } }),
                ]
            }
            _ => return Err(invalid("Command Code 不支持此消息角色")),
        };
        // 同批并行工具的结果一起回灌，保持结果的原顺序和每个调用id，不删掉任何结果。
        if role == "tool" {
            if let Some(last) = output.last_mut().filter(|last| last["role"] == "tool") {
                if let Some(parts) = last["content"].as_array_mut() {
                    parts.extend(content);
                    continue;
                }
            }
        }
        output.push(json!({ "role": role, "content": content }));
    }
    Ok(output)
}

fn user_content(content: Option<&Value>) -> Result<Vec<Value>> {
    match content {
        Some(Value::String(text)) => Ok(vec![json!({ "type": "text", "text": text })]),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| match part["type"].as_str() {
                Some("text") => {
                    if !part["text"].is_string() {
                        return Err(invalid("Command Code 文本块缺少文本"));
                    }
                    Ok(part.clone())
                }
                Some("image_url" | "image") => image(part),
                _ => Err(invalid("Command Code 消息包含未接通的内容块，未静默丢弃")),
            })
            .collect(),
        None | Some(Value::Null) => Ok(Vec::new()),
        _ => Err(invalid("Command Code 消息内容必须是文本或内容块数组")),
    }
}

fn image(part: &Value) -> Result<Value> {
    let url = if part["type"] == "image_url" {
        part["image_url"]
            .as_str()
            .or_else(|| part["image_url"]["url"].as_str())
            .map(str::to_string)
    } else if let Some(url) = part["image"].as_str() {
        Some(url.to_string())
    } else {
        match part["source"]["type"].as_str() {
            Some("url") => part["source"]["url"].as_str().map(str::to_string),
            Some("base64") => part["source"]["media_type"]
                .as_str()
                .zip(part["source"]["data"].as_str())
                .map(|(mime, data)| format!("data:{mime};base64,{data}")),
            _ => None,
        }
    }
    .filter(|url| !url.is_empty())
    .ok_or_else(|| invalid("Command Code 图片块缺少地址或内联数据"))?;
    let mut block = json!({ "type": "image", "image": url });
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some(end) = rest.find([';', ',']) {
            let mime = rest[..end].trim();
            if !mime.starts_with("image/") {
                return Err(invalid("Command Code 只接受图片内联数据"));
            }
            block["mimeType"] = json!(mime);
        }
    } else if !url.starts_with("https://") {
        return Err(invalid(
            "Command Code 图片只接受 HTTPS 公网地址或内联图片，不读取本机路径",
        ));
    }
    for key in ["cache_control", "providerOptions"] {
        if let Some(value) = part.get(key) {
            block[key] = value.clone();
        }
    }
    Ok(block)
}

fn assistant_content(message: &Value) -> Result<Vec<Value>> {
    let mut parts = Vec::new();
    let reasoning = message
        .get("reasoning_content")
        .filter(|value| truthy(value))
        .or_else(|| message.get("reasoning").filter(|value| truthy(value)))
        .map(string)
        .filter(|value| !value.is_empty());
    if let Some(text) = &reasoning {
        parts.push(json!({ "type": "reasoning", "text": text }));
    }
    match message.get("content") {
        Some(Value::String(text)) if !text.is_empty() => {
            parts.push(json!({ "type": "text", "text": text }))
        }
        Some(Value::Array(items)) => {
            for item in items {
                match item["type"].as_str() {
                    Some("text") => parts.push(item.clone()),
                    Some("reasoning") if reasoning.is_none() => parts.push(item.clone()),
                    Some("thinking") if reasoning.is_none() => {
                        let mut block = item.clone();
                        block["type"] = json!("reasoning");
                        block["text"] = item
                            .get("thinking")
                            .or_else(|| item.get("text"))
                            .cloned()
                            .unwrap_or(json!(""));
                        if let Some(fields) = block.as_object_mut() {
                            fields.remove("thinking");
                        }
                        parts.push(block);
                    }
                    Some("reasoning" | "thinking") => {}
                    _ => return Err(invalid("Command Code 历史助手内容块尚未接通，未静默丢弃")),
                }
            }
        }
        _ => {}
    }
    if message
        .get("tool_calls")
        .is_some_and(|value| !value.is_null() && !value.is_array())
    {
        return Err(invalid("Command Code 工具调用必须是数组"));
    }
    for call in message["tool_calls"].as_array().into_iter().flatten() {
        let id = call["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| invalid("Command Code 工具调用缺少标识"))?;
        let name = call["function"]["name"]
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| invalid("Command Code 工具调用缺少名称"))?;
        let input = match call.pointer("/function/arguments") {
            Some(Value::String(text)) => serde_json::from_str::<Value>(text)
                .ok()
                .filter(Value::is_object)
                .ok_or_else(|| invalid("Command Code 工具参数不是有效 JSON 对象"))?,
            Some(value) if value.is_object() => value.clone(),
            None | Some(Value::Null) => json!({}),
            _ => return Err(invalid("Command Code 工具参数必须是 JSON 对象")),
        };
        parts.push(
            json!({ "type": "tool-call", "toolCallId": id, "toolName": name, "input": input }),
        );
    }
    Ok(parts)
}

fn tool_output(content: Option<&Value>) -> Result<String> {
    match content {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| {
                if part["type"] != "text" || !part["text"].is_string() {
                    return Err(invalid(
                        "Command Code 工具结果目前只接通文本块，未静默删除其它结果",
                    ));
                }
                Ok(part["text"].as_str().unwrap_or("").to_string())
            })
            .collect::<Result<Vec<_>>>()
            .map(|parts| parts.join("\n")),
        Some(value) => Ok(string(value)),
        None => Ok(String::new()),
    }
}

pub fn tools(body: &Value) -> Result<Vec<Value>> {
    if body
        .get("tools")
        .is_some_and(|value| !value.is_null() && !value.is_array())
    {
        return Err(invalid("Command Code 工具声明必须是数组"));
    }
    body["tools"].as_array().into_iter().flatten().map(|tool| {
        if tool.get("type").is_some_and(|value| value != "function") {
            return Err(invalid("Command Code 尚未接通此工具类型"));
        }
        let field = |nested: &str, flat: &str| tool.get("function").and_then(|value| value.get(nested)).or_else(|| tool.get(flat));
        let name = field("name", "name").and_then(Value::as_str).filter(|name| !name.is_empty())
            .ok_or_else(|| invalid("Command Code 工具声明缺少名称"))?;
        let schema = field("parameters", "input_schema").cloned().unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
        if !schema.is_object() { return Err(invalid("Command Code 工具输入结构必须是 JSON 对象")); }
        Ok(json!({ "name": name,
            "description": field("description", "description").and_then(Value::as_str).unwrap_or(""),
            "input_schema": schema }))
    }).collect()
}
