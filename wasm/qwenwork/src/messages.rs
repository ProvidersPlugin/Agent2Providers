//! OpenAI 消息/工具规范化，移植当前 千问办公 原生规则，不运行任何工具。
//! 用户图片保留 image_url；显式失败的助手轮次连同对应工具结果一起移除。
use super::util;
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                part.as_str()
                    .or_else(|| part["text"].as_str())
                    .or_else(|| part["content"].as_str())
            })
            .collect(),
        _ => String::new(),
    }
}
fn reasoning(row: &Value) -> String {
    for key in ["reasoning_content", "reasoning"] {
        if let Some(text) = row[key].as_str().filter(|text| !text.trim().is_empty()) { return text.to_string(); }
    }
    row["reasoning_details"].as_array().into_iter().flatten()
        .filter_map(|part| part["text"].as_str().or_else(|| part["summary"].as_str())).collect()
}
pub fn normalize(body: &Value) -> Result<Vec<Value>> {
    let raw = body["messages"]
        .as_array()
        .filter(|rows| !rows.is_empty())
        .ok_or_else(|| Error::http(400, "千问办公 请求缺少有效消息数组"))?;
    let mut messages = Vec::new();
    let mut systems = Vec::new();
    let mut dropped = Vec::new();
    for row in raw {
        let role = row["role"]
            .as_str()
            .ok_or_else(|| Error::http(400, "千问办公 消息角色无效"))?;
        if role == "assistant" && (util::truthy(&row["__failed"]) || util::truthy(&row["__aborted"])) {
            dropped.extend(
                row["tool_calls"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|call| call["id"].as_str())
                    .map(str::to_string),
            );
            continue;
        }
        if role == "tool" && dropped.iter().any(|id| row["tool_call_id"] == *id) {
            continue;
        }
        match role {
            "system" | "developer" => {
                let content = text(&row["content"]);
                if !content.is_empty() {
                    systems.push(json!({ "role": "system", "content": content }));
                }
            }
            "user" => {
                let images = row["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|part| part["type"] == "image_url")
                    .cloned()
                    .collect::<Vec<_>>();
                if images.is_empty() {
                    messages.push(json!({ "role": "user", "content": text(&row["content"]) }));
                } else {
                    let mut parts = Vec::new();
                    let content = text(&row["content"]);
                    if !content.is_empty() {
                        parts.push(json!({ "type": "text", "text": content }));
                    }
                    for image in images {
                        let url = image["image_url"]
                            .as_str()
                            .or_else(|| image["image_url"]["url"].as_str())
                            .ok_or_else(|| Error::http(400, "千问办公 图片地址格式无效"))?;
                        if url.is_empty()
                            || url.chars().any(char::is_control)
                            || !(url.starts_with("data:image/")
                                || url.starts_with("https://")
                                || url.starts_with("http://"))
                        {
                            return Err(Error::http(400, "千问办公 只接受图片 data URL 或 HTTP 图片地址"));
                        }
                        parts.push(json!({ "type": "image_url", "image_url": image["image_url"] }));
                    }
                    messages.push(json!({ "role": "user", "content": parts }));
                }
            }
            "assistant" => {
                let mut content = text(&row["content"]);
                let mut calls = Vec::new();
                for call in row["tool_calls"].as_array().into_iter().flatten() {
                    let id = call["id"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| Error::http(400, "千问办公 工具调用缺少标识"))?;
                    let name = call["function"]["name"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| Error::http(400, "千问办公 工具调用缺少名称"))?;
                    let arguments = match call["function"].get("arguments") {
                        Some(Value::String(value)) => value.clone(),
                        Some(value) => value.to_string(),
                        None => "{}".into(),
                    };
                    calls.push(
                        json!({ "id": id, "type": "function", "function": { "name": name, "arguments": arguments } }),
                    );
                }
                if content.is_empty() && !calls.is_empty() {
                    content.push(' ');
                }
                let mut message = json!({ "role": "assistant", "content": content });
                let reasoning = reasoning(row);
                if !reasoning.is_empty() { message["reasoning_content"] = json!(reasoning); }
                if !calls.is_empty() {
                    message["tool_calls"] = json!(calls);
                }
                messages.push(message);
            }
            "tool" => {
                let id = row["tool_call_id"].as_str().filter(|id| !id.is_empty() && id.len() <= 512 && !id.chars().any(char::is_control))
                    .ok_or_else(|| Error::http(400, "千问办公工具结果缺少有效调用标识"))?;
                messages.push(json!({ "role": "tool", "tool_call_id": id, "content": text(&row["content"]) }));
            }
            _ => return Err(Error::http(400, "千问办公 不支持当前消息角色，未静默丢弃消息")),
        }
    }
    systems.extend(messages);
    Ok(systems)
}
pub fn tools(body: &Value) -> Result<Vec<Value>> {
    if body["tool_choice"] == "none" { return Ok(Vec::new()); }
    let Some(value) = body.get("tools").filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let tools = value
        .as_array()
        .ok_or_else(|| Error::http(400, "千问办公 tools 必须是数组"))?;
    let mut output = Vec::new();
    for tool in tools {
        let function = tool["function"]
            .as_object()
            .filter(|function| {
                function
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| !name.is_empty())
            })
            .ok_or_else(|| Error::http(400, "千问办公 只支持有效的函数工具定义"))?;
        let mut mapped = json!({ "name": function.get("name") });
        for key in ["description", "parameters", "strict"] {
            if let Some(value) = function.get(key) {
                mapped[key] = value.clone();
            }
        }
        output.push(json!({ "type": "function", "function": mapped }));
    }
    Ok(output)
}
