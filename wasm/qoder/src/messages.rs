//! OpenAI 消息/工具规范化，移植当前 Qoder 原生规则，不运行任何工具。
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
pub fn normalize(body: &Value) -> Result<Vec<Value>> {
    let raw = body["messages"]
        .as_array()
        .filter(|rows| !rows.is_empty())
        .ok_or_else(|| Error::http(400, "Qoder 请求缺少有效消息数组"))?;
    let mut messages = Vec::new();
    let mut systems = Vec::new();
    let mut dropped = Vec::new();
    for row in raw {
        let role = row["role"]
            .as_str()
            .ok_or_else(|| Error::http(400, "Qoder 消息角色无效"))?;
        if role == "assistant"
            && (util::truthy(&row["__failed"]) || util::truthy(&row["__aborted"]))
        {
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
                let content = neutralize_system_openers(&text(&row["content"]));
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
                            .ok_or_else(|| Error::http(400, "Qoder 图片地址格式无效"))?;
                        if url.is_empty()
                            || url.chars().any(char::is_control)
                            || !(url.starts_with("data:image/")
                                || url.starts_with("https://")
                                || url.starts_with("http://"))
                        {
                            return Err(Error::http(
                                400,
                                "Qoder 只接受图片 data URL 或 HTTP 图片地址",
                            ));
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
                        .ok_or_else(|| Error::http(400, "Qoder 工具调用缺少标识"))?;
                    let name = call["function"]["name"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| Error::http(400, "Qoder 工具调用缺少名称"))?;
                    let arguments = match call["function"].get("arguments") {
                        Some(Value::String(value)) => value.clone(),
                        Some(value) => value.to_string(),
                        None => "{}".into(),
                    };
                    calls.push(json!({ "id": id, "type": "function", "function": { "name": name, "arguments": arguments } }));
                }
                if content.is_empty() && !calls.is_empty() {
                    content.push(' ');
                }
                let mut message = json!({ "role": "assistant", "content": content });
                if let Some(reasoning) = row
                    .get("reasoning_content")
                    .filter(|value| value.is_string())
                {
                    message["reasoning_content"] = reasoning.clone();
                }
                if !calls.is_empty() {
                    message["tool_calls"] = json!(calls);
                }
                messages.push(message);
            }
            "tool" => {
                let id = row["tool_call_id"]
                    .as_str()
                    .filter(|value| {
                        !value.is_empty() && value.len() <= 512 && !value.contains('\0')
                    })
                    .ok_or_else(|| Error::http(400, "Qoder 工具结果缺少有效调用标识"))?;
                messages.push(
                    json!({ "role": "tool", "tool_call_id": id, "content": text(&row["content"]) }),
                );
            }
            _ => return Err(Error::http(400, "Qoder 不支持当前消息角色，未静默丢弃消息")),
        }
    }
    systems.extend(messages);
    Ok(systems)
}
pub fn tools(body: &Value) -> Result<Vec<Value>> {
    let Some(value) = body.get("tools").filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let tools = value
        .as_array()
        .ok_or_else(|| Error::http(400, "Qoder tools 必须是数组"))?;
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
            .ok_or_else(|| Error::http(400, "Qoder 只支持有效的函数工具定义"))?;
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

const AGENT_SDK_LINE: &str = "You are a Claude agent, built on Anthropic's Claude Agent SDK.";

/// 仅对 system/developer 中的第三方客户端固定开场句做最小改写，避免触发 Qoder 上游假排队（10605/p3）或 400。
/// 不改动 user/assistant/tool 消息，其余系统提示词逐字保留。
pub fn neutralize_system_openers(text: &str) -> String {
    let mut out = replace_ascii_ci(
        text,
        "you are claude code, anthropic's official cli for claude, running within the claude agent sdk.",
        AGENT_SDK_LINE,
    );
    out = replace_ascii_ci(
        &out,
        "you are claude code, anthropic's official cli for claude.",
        AGENT_SDK_LINE,
    );
    out = replace_ascii_ci(
        &out,
        "you are claude code, anthropic's official cli tool for claude.",
        AGENT_SDK_LINE,
    );
    out = replace_ascii_ci(
        &out,
        "you are zcode, an interactive agent",
        "You are an interactive coding agent",
    );
    out = replace_ascii_ci(&out, "you are kimi code,", "You are a coding agent,");
    out = replace_ascii_ci(
        &out,
        "you are codex, based on",
        "You are a coding agent, based on",
    );
    insert_harness_comma(&out)
}

fn replace_ascii_ci(input: &str, needle_lower: &str, replacement: &str) -> String {
    let lower = input.to_ascii_lowercase();
    if !lower.contains(needle_lower) {
        return input.to_string();
    }
    let mut result = String::with_capacity(input.len());
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find(needle_lower) {
        let start = pos + rel;
        let end = start + needle_lower.len();
        result.push_str(&input[pos..start]);
        result.push_str(replacement);
        pos = end;
    }
    result.push_str(&input[pos..]);
    result
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn insert_harness_comma(input: &str) -> String {
    const CONNECTORS: [&str; 5] = [
        "powered by",
        "running on",
        "running in",
        "built on",
        "based on",
    ];
    let lower = input.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut result = String::with_capacity(input.len() + 8);
    let mut cursor = 0;
    let mut search = 0;
    while let Some(rel) = lower[search..].find("you are ") {
        let start = search + rel;
        if start > 0 && is_word_byte(bytes[start - 1]) {
            search = start + 8;
            continue;
        }
        let after_prefix = start + 8;
        let article_end = ["an ", "a ", "the "]
            .iter()
            .find_map(|article| {
                lower[after_prefix..]
                    .starts_with(article)
                    .then_some(after_prefix + article.len())
            });
        let Some(scan_start) = article_end else {
            search = after_prefix;
            continue;
        };
        let mut inserted = false;
        let mut idx = scan_start;
        while idx < bytes.len() && idx.saturating_sub(scan_start) <= 80 {
            if matches!(bytes[idx], b'\n' | b'\r' | b'.' | b',') {
                break;
            }
            let role_len = if lower[idx..].starts_with("agent") {
                Some(5)
            } else if lower[idx..].starts_with("assistant") {
                Some(9)
            } else {
                None
            };
            if let Some(len) = role_len {
                let before_ok = idx == 0 || !is_word_byte(bytes[idx - 1]);
                let after_role = idx + len;
                let after_ok = after_role < bytes.len() && bytes[after_role].is_ascii_whitespace();
                if before_ok && after_ok {
                    let mut conn_start = after_role;
                    while conn_start < bytes.len()
                        && matches!(bytes[conn_start], b' ' | b'\t')
                    {
                        conn_start += 1;
                    }
                    let matched = CONNECTORS.iter().find(|conn| {
                        lower[conn_start..].starts_with(*conn)
                            && bytes
                                .get(conn_start + conn.len())
                                .is_none_or(|b| !is_word_byte(*b))
                    });
                    if matched.is_some() {
                        result.push_str(&input[cursor..after_role]);
                        result.push_str(", ");
                        cursor = conn_start;
                        search = conn_start;
                        inserted = true;
                        break;
                    }
                }
            }
            idx += 1;
            while idx < bytes.len() && !input.is_char_boundary(idx) {
                idx += 1;
            }
        }
        if !inserted {
            search = after_prefix;
        }
    }
    result.push_str(&input[cursor..]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_known_qoder_trapped_openers_only() {
        assert_eq!(
            neutralize_system_openers("You are an AI agent powered by DeepSeek Harness."),
            "You are an AI agent, powered by DeepSeek Harness."
        );
        assert_eq!(
            neutralize_system_openers("You are Claude Code, Anthropic's official CLI for Claude."),
            AGENT_SDK_LINE
        );
        assert_eq!(
            neutralize_system_openers(
                "You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK."
            ),
            AGENT_SDK_LINE
        );
        assert_eq!(
            neutralize_system_openers("You are ZCode, an interactive agent working on code."),
            "You are an interactive coding agent working on code."
        );
        assert_eq!(
            neutralize_system_openers(
                "You are Kimi Code, an interactive general AI agent running on a user's computer."
            ),
            "You are a coding agent, an interactive general AI agent running on a user's computer."
        );
        assert_eq!(
            neutralize_system_openers("You are Codex, based on GPT-5."),
            "You are a coding agent, based on GPT-5."
        );
        assert_eq!(
            neutralize_system_openers("You are a helpful assistant."),
            "You are a helpful assistant."
        );
    }
}
