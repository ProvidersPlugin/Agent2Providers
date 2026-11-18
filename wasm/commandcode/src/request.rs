//! 逐字段保留原生 plan.rs 的 Command Code 1.53.1 请求信封和会话派生。
//! 不把 OpenAI 请求直接透传；系统提示、工具回灌、缓存标记与设备档案保持原语义。

#[path = "messages.rs"]
mod messages;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use sha1::{Digest, Sha1};
use std::collections::BTreeMap;

use super::{
    config::{Config, DEVICE_PLATFORM},
    fingerprint,
};

pub const DEFAULT_MODEL: &str = "deepseek/deepseek-v4-flash";

pub struct Request {
    pub url: String,
    pub headers: Value,
    pub body: Value,
    pub model: String,
}

pub(super) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        _ => true,
    }
}

pub(super) fn string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        _ => value.to_string(),
    }
}

pub fn build(key: &str, body: &Value, headers: &Value, config: &Config) -> Result<Request> {
    if !body.is_object() {
        return Err(Error::http(400, "Command Code 请求体必须是 JSON 对象"));
    }
    let messages = body["messages"]
        .as_array()
        .filter(|messages| !messages.is_empty() && messages.len() <= 512)
        .ok_or_else(|| Error::http(400, "Command Code 请求必须包含有效消息数组"))?
        .clone();
    let model = body["model"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_MODEL)
        .to_string();
    let mut system = messages::system_blocks(&messages);
    for index in 0..system.len().saturating_sub(1) {
        if let Some(text) = system
            .get_mut(index)
            .and_then(|block| block.get_mut("text"))
        {
            *text = json!(format!("{}\n", text.as_str().unwrap_or("")));
        }
    }
    // 缺省不能让上游注入 CLI 自带的长系统提示，必须给一个空格占位块。
    if system.is_empty() {
        system.push(json!({ "type": "text", "text": " " }));
    }
    let prompt_cache_key = body["prompt_cache_key"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let converted = messages::convert(&messages)?;
    let client_headers: BTreeMap<String, String> = headers
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, value)| {
            value
                .as_str()
                .map(|value| (name.to_ascii_lowercase(), value.to_string()))
        })
        .collect();
    let session = session_id(
        key,
        &model,
        &system,
        &messages,
        prompt_cache_key,
        &client_headers,
    );
    let cache_marked = system.iter().any(|block| truthy(&block["cache_control"]))
        || converted.iter().any(|message| {
            message["content"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|part| truthy(&part["cache_control"])))
        });
    if prompt_cache_key.is_some() && !cache_marked {
        if let Some(last) = system.last_mut() {
            last["cache_control"] = json!({ "type": "ephemeral" });
        }
    }
    let mut params = json!({ "model": model, "messages": converted, "system": system,
        "stream": true, "max_tokens": max_tokens(body) });
    if let Some(value) = body.get("temperature").filter(|value| value.is_number()) {
        params["temperature"] = value.clone();
    }
    if let Some(value) = body["reasoning_effort"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        params["reasoning_effort"] = json!(value);
    }
    // CLI wire保留模型的原生思考扩展，不能只留下reasoning_effort而丢掉客户端显式开关。
    for key in ["reasoning", "thinking", "effort", "output_config"] {
        if let Some(value) = body.get(key) {
            params[key] = value.clone();
        }
    }
    let mut tools = messages::tools(body)?;
    if let Some(choice) = body.get("tool_choice") {
        match tool_choice(choice)? {
            Some(choice) => params["tool_choice"] = choice,
            None => tools.clear(),
        }
    }
    params["tools"] = json!(tools);
    if let Some(value) = body["parallel_tool_calls"].as_bool() {
        params["parallel_tool_calls"] = json!(value);
    }
    let date = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(sdk::now())
        .ok_or_else(|| Error::new("宿主时钟超出有效范围"))?
        .format("%Y-%m-%d")
        .to_string();
    let mut envelope = json!({
        "config": { "workingDir": config.project_dir, "date": date, "environment": DEVICE_PLATFORM,
            "structure": [], "isGitRepo": false, "currentBranch": "", "mainBranch": "", "gitStatus": "", "recentCommits": [] },
        "memory": null, "taste": null, "skills": null, "permissionMode": "standard",
        "mode": "agent", "params": params
    });
    if wire_uuid(&session) {
        envelope["threadId"] = json!(session);
    }
    let mut headers = config.headers(key);
    if let Some(headers) = headers.as_object_mut() {
        headers.remove("accept");
    }
    headers["x-project-slug"] = json!(fingerprint::slugify_project_path(&config.project_dir));
    headers["x-taste-learning"] = json!("false");
    headers["x-session-id"] = json!(session);
    headers["traceparent"] = json!(fingerprint::traceparent()?);
    if client_headers
        .get("x-cmd-zdr")
        .is_some_and(|value| value.trim() == "1")
    {
        headers["x-cmd-zdr"] = json!("1");
    }
    Ok(Request {
        url: config.url("/alpha/generate"),
        headers,
        body: envelope,
        model,
    })
}

fn max_tokens(body: &Value) -> i64 {
    body.get("max_completion_tokens")
        .or_else(|| body.get("max_tokens"))
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_f64().map(|value| value as i64))
        })
        .filter(|value| *value > 0)
        .unwrap_or(64_000)
        .min(200_000)
}

fn tool_choice(choice: &Value) -> Result<Option<Value>> {
    let invalid = || {
        Error::http(
            400,
            "Command Code 工具选择必须是 auto、required、none 或明确工具名称",
        )
    };
    Ok(match choice {
        Value::String(value) => match value.trim() {
            "none" => None,
            "required" | "any" => Some(json!({ "type": "any" })),
            "auto" => Some(json!({ "type": "auto" })),
            _ => return Err(invalid()),
        },
        Value::Object(object) => match object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
        {
            "none" => None,
            "function" => {
                let name = choice["function"]["name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .ok_or_else(invalid)?;
                Some(json!({ "type": "tool", "name": name }))
            }
            "tool" => {
                let name = choice["name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .ok_or_else(invalid)?;
                Some(json!({ "type": "tool", "name": name }))
            }
            "auto" | "any" => Some(choice.clone()),
            _ => return Err(invalid()),
        },
        Value::Null => Some(json!({ "type": "auto" })),
        _ => return Err(invalid()),
    })
}

fn session_id(
    key: &str,
    model: &str,
    system: &[Value],
    messages: &[Value],
    prompt_cache_key: Option<&str>,
    headers: &BTreeMap<String, String>,
) -> String {
    for name in ["x-session-id", "x-claude-code-session-id", "session_id"] {
        if let Some(value) = headers.get(name).map(|value| value.trim()).filter(|value| {
            (8..=256).contains(&value.len()) && !value.chars().any(char::is_control)
        }) {
            return value.to_string();
        }
    }
    if let Some(value) = prompt_cache_key
        .filter(|value| (8..=256).contains(&value.len()) && !value.chars().any(char::is_control))
    {
        return value.to_string();
    }
    // 保留旧SHA-1/NUL分隔算法，修正字符串和系统块后的首轮用户内容缺失。
    let mut hasher = Sha1::new();
    for value in [key, model] {
        hasher.update(value.as_bytes());
        hasher.update([0u8]);
    }
    for block in system {
        if let Some(text) = block["text"].as_str() {
            hasher.update(text.as_bytes());
            hasher.update([0u8]);
        }
    }
    // 首个用户轮次可能在system/developer之后，字符串和块数组都参与派生，不能跨会话撞同一thread。
    let mut found_user = false;
    for message in messages {
        if matches!(message["role"].as_str(), Some("system" | "developer")) {
            continue;
        }
        if message["role"] != "user" {
            break;
        }
        found_user = true;
        match &message["content"] {
            Value::String(text) => {
                hasher.update(text.as_bytes());
                hasher.update([0u8]);
            }
            Value::Array(parts) => {
                for part in parts {
                    if part["type"] == "text" {
                        if let Some(text) = part["text"].as_str() {
                            hasher.update(text.as_bytes());
                            hasher.update([0u8]);
                        }
                    } else if matches!(part["type"].as_str(), Some("image_url" | "image")) {
                        hasher.update(part.to_string().as_bytes());
                        hasher.update([0u8]);
                    }
                }
            }
            _ => {}
        }
    }
    if !found_user {
        hasher.update(b"no-user");
    }
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = fingerprint::hex(&bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

fn wire_uuid(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    parts.len() == 5
        && parts.iter().zip([8, 4, 4, 4, 12]).all(|(part, length)| {
            part.len() == length && part.chars().all(|ch| ch.is_ascii_hexdigit())
        })
}
