//! 国内 qodercli 固定聊天信封；system 放 messages，model_config 不带 thinking_config。
//! 记录指纹保持原生规则，会话只属于当前用户/模型；所有随机 ID 来自宿主。
use super::{catalog, context, credentials::Credentials, messages, reasoning, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};
use sha2::{Digest, Sha256};

pub struct Payload {
    pub value: Value,
    pub thinking: bool,
}
pub fn build(
    body: &Value,
    model: &Value,
    credentials: &Credentials,
    session: Option<&str>,
) -> Result<Payload> {
    let key = model["upstreamKey"].as_str().unwrap_or("");
    let config = &model["config"];
    let messages = messages::normalize(body)?;
    let tools = messages::tools(body)?;
    let thinking = reasoning::resolve(body, model);
    for field in ["max_tokens", "max_completion_tokens", "max_thinking_tokens"] {
        if body.get(field).is_some_and(|value| {
            !value.is_null()
                && value
                    .as_u64()
                    .is_none_or(|number| field != "max_thinking_tokens" && number == 0)
        }) {
            return Err(Error::http(400, format!("Qoder {field} 必须是有效整数")));
        }
    }
    if body
        .get("n")
        .is_some_and(|value| !value.is_null() && value.as_u64() != Some(1))
    {
        return Err(Error::http(400, "Qoder 当前对话通道每次只返回一条回答"));
    }
    if body
        .get("parallel_tool_calls")
        .is_some_and(|value| !value.is_null() && !value.is_boolean())
    {
        return Err(Error::http(400, "Qoder parallel_tool_calls 必须为布尔值"));
    }
    let limit = body["max_tokens"]
        .as_u64()
        .or_else(|| body["max_completion_tokens"].as_u64())
        .unwrap_or(catalog::MAX_OUTPUT as u64)
        .min(catalog::MAX_OUTPUT as u64) as i64;
    let record = record_id(key, &messages, &tools, limit);
    let seed = session
        .filter(|value| !value.is_empty())
        .or_else(|| body["session_id"].as_str())
        .or_else(|| body["user"].as_str());
    let session = session_id(&credentials.user, key, seed)?;
    let last = messages
        .iter()
        .rev()
        .find(|row| row["role"] == "user")
        .map(|row| messages::text(&row["content"]))
        .unwrap_or_default();
    let mut parameters = json!({ "max_tokens": limit });
    for field in [
        "temperature",
        "top_p",
        "presence_penalty",
        "frequency_penalty",
        "stop",
        "seed",
        "logit_bias",
        "response_format",
        "max_thinking_tokens",
        "tool_choice",
        "n",
    ] {
        if let Some(value) = body.get(field).filter(|value| !value.is_null()) {
            parameters[field] = value.clone();
        }
    }
    if let Some(true) = thinking.enabled {
        parameters["enable_thinking"] = json!(true);
        if let Some(effort) = &thinking.effort {
            parameters["reasoning_effort"] = json!(effort);
        }
    }
    let mut slim = json!({ "key": key });
    for field in ["is_reasoning", "is_vl", "source", "format"] {
        if let Some(value) = config.get(field).filter(|value| !value.is_null()) {
            slim[field] = value.clone();
        }
    }
    let mut payload = json!({ "request_id": util::uuid()?, "request_set_id": record, "chat_record_id": record,
        "session_id": session, "stream": true, "chat_task": "FREE_INPUT", "is_reply": true,
        "is_retry": false, "source": 1, "version": "3", "session_type": "qodercli", "agent_id": "agent_common",
        "task_id": "common", "code_language": "", "chat_prompt": "", "image_urls": null, "aliyun_user_type": "", "system": "",
        "messages": messages, "tools": tools, "parameters": parameters, "model_config": slim,
        "chat_context": { "chatPrompt": "", "imageUrls": null, "extra": { "context": [],
            "modelConfig": { "key": key, "is_reasoning": util::truthy(&config["is_reasoning"]) }, "originalContent": last },
            "features": [], "text": last },
        "business": { "product": "cli", "version": "1.0.0", "type": "agent", "stage": "start",
            "id": util::uuid()?, "name": last.chars().take(30).collect::<String>(), "begin_at": util::now()?, "model_key": key } });
    if let Some(value) = body
        .get("parallel_tool_calls")
        .filter(|value| !value.is_null())
    {
        payload["parallel_tool_calls"] = value.clone();
    }
    context::apply(&mut payload, config, &messages, &tools);
    Ok(Payload {
        value: payload,
        thinking: thinking.enabled.is_some() || thinking.effort.is_some(),
    })
}
fn session_id(user: &str, model: &str, seed: Option<&str>) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"qoder-session");
    hasher.update([0]);
    hasher.update(user.as_bytes());
    hasher.update([0]);
    hasher.update(model.as_bytes());
    let base = format!("{:x}", hasher.finalize());
    let suffix = match seed.filter(|value| !value.is_empty()) {
        Some(value) => value.to_string(),
        None => util::uuid()?,
    };
    Ok(format!("{}-{suffix}", &base[..16]))
}
fn record_id(model: &str, messages: &[Value], tools: &[Value], limit: i64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"qoder-record");
    hasher.update([0]);
    hasher.update(model.as_bytes());
    for row in messages {
        hasher.update([0]);
        hasher.update(row.to_string().as_bytes());
    }
    if !tools.is_empty() {
        hasher.update([0]);
        hasher.update(json!(tools).to_string().as_bytes());
    }
    hasher.update([0]);
    hasher.update(format!("mt={limit}").as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}
