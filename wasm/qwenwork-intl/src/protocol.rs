//! 国际业务信封按原生千问基线推断保留，非国际实测；qoder_work/processing 与 COSY 六号客户端一致。
//! model_config 保留千问宽形状、剥离 thinking_config；两处上下文长度必须与当前目录一致。
use super::{context, credentials::Credentials, endpoints, messages, reasoning, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};
use sha2::{Digest, Sha256};

pub struct Payload {
    pub value: Value,
    pub thinking: bool,
}
pub fn build(body: &Value, model: &Value, credentials: &Credentials) -> Result<Payload> {
    let key = model["upstreamKey"].as_str().unwrap_or("");
    let config = &model["config"];
    let messages = messages::normalize(body)?;
    let tools = messages::tools(body)?;
    let thinking = reasoning::resolve(body, model);
    let limit = body["max_tokens"]
        .as_i64()
        .or_else(|| body["max_completion_tokens"].as_i64())
        .filter(|value| *value > 0)
        .unwrap_or(endpoints::MAX_OUTPUT)
        .min(endpoints::MAX_OUTPUT);
    let record = record_id(key, &messages, &tools, limit);
    let seed = body["user"]
        .as_str()
        .or_else(|| body["session_id"].as_str());
    let session = session_id(&credentials.user, key, seed)?;
    let last = messages
        .iter()
        .rev()
        .find(|row| row["role"] == "user")
        .map(|row| messages::text(&row["content"]))
        .unwrap_or_default();
    let declared = config["max_input_tokens"]
        .as_i64()
        .filter(|value| *value > 0)
        .unwrap_or(endpoints::REMOTE_CONTEXT_FALLBACK);
    let mut parameters = json!({ "max_tokens": limit, "context_length": declared });
    for key in [
        "temperature",
        "top_p",
        "presence_penalty",
        "frequency_penalty",
    ] {
        if let Some(value) = body.get(key).filter(|v| !v.is_null()) {
            if util::number(value).is_none() {
                return Err(Error::http(400, format!("千问办公 {key} 必须是有限数值")));
            }
            parameters[key] = value.clone();
        }
    }
    if let Some(stop) = body.get("stop").filter(|v| !v.is_null()) {
        if !stop.is_string()
            && !stop
                .as_array()
                .is_some_and(|values| values.iter().all(Value::is_string))
        {
            return Err(Error::http(400, "千问办公 stop 必须是文本或文本数组"));
        }
        parameters["stop"] = stop.clone();
    }
    if let Some(enabled) = thinking.enabled {
        parameters["enable_thinking"] = json!(enabled);
        if enabled {
            if let Some(effort) = &thinking.effort {
                parameters["reasoning_effort"] = json!(effort);
            }
        }
    }
    let source = config["source"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or("system");
    let format = config["format"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or("openai");
    let shaped = json!({ "key": key, "display_name": key, "model": "", "format": format,
        "is_vl": util::truthy(&config["is_vl"]), "is_reasoning": util::truthy(&config["is_reasoning"]),
        "api_key": "", "url": "", "source": source, "max_input_tokens": declared });
    let mut payload = json!({ "request_id": util::uuid()?, "request_set_id": record, "chat_record_id": record,
        "session_id": session, "stream": true, "chat_task": "FREE_INPUT", "is_reply": true,
        "is_retry": false, "source": 1, "version": "3", "session_type": endpoints::PRODUCT, "agent_id": "agent_common",
        "task_id": "common", "code_language": "", "chat_prompt": "", "image_urls": null, "aliyun_user_type": "", "system": "",
        "messages": messages, "tools": tools, "parameters": parameters, "model_config": shaped,
        "chat_context": { "chatPrompt": "", "imageUrls": null, "extra": { "context": [],
            "modelConfig": { "key": key, "is_reasoning": util::truthy(&config["is_reasoning"]) }, "originalContent": last },
            "features": [], "text": last },
        "business": { "product": endpoints::PRODUCT, "version": endpoints::COSY_VERSION, "type": endpoints::BUSINESS_TYPE,
            "stage": endpoints::STAGE, "id": util::uuid()?, "name": last.chars().take(30).collect::<String>(), "begin_at": util::now()? } });
    if let Some(choice) = body.get("tool_choice").filter(|v| !v.is_null()) {
        if tools.is_empty() && choice != "none" {
            return Err(Error::http(400, "千问办公指定工具前必须提供工具定义"));
        }
        if !tools.is_empty() {
            payload["tool_choice"] = choice.clone();
        }
    }
    if let Some(parallel) = body.get("parallel_tool_calls").filter(|v| !v.is_null()) {
        if !parallel.is_boolean() {
            return Err(Error::http(
                400,
                "千问办公 parallel_tool_calls 必须是布尔值",
            ));
        }
        payload["parallel_tool_calls"] = parallel.clone();
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
    hasher.update(endpoints::PROVIDER.as_bytes());
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
    // 记录内容指纹沿用原生公式；会话编号另有 provider/账号隔离，不复用其它家状态。
    hasher.update(b"qoder-record");
    hasher.update([0]);
    hasher.update(model.as_bytes());
    for row in messages {
        if let Some(role) = row["role"].as_str() {
            hasher.update([0]);
            hasher.update(role.as_bytes());
        }
        if let Some(content) = row.get("content").filter(|value| !value.is_null()) {
            hasher.update([0]);
            match content {
                Value::String(value) => hasher.update(value.as_bytes()),
                other => hasher.update(other.to_string().as_bytes()),
            }
        }
        for key in [
            "tool_calls",
            "tool_call_id",
            "reasoning_content",
            "reasoning",
            "thinking",
        ] {
            if let Some(value) = row.get(key).filter(|value| !value.is_null()) {
                hasher.update([0]);
                hasher.update(key.as_bytes());
                hasher.update([0]);
                hasher.update(value.to_string().as_bytes());
            }
        }
    }
    if !tools.is_empty() {
        hasher.update([0]);
        hasher.update(json!(tools).to_string().as_bytes());
    }
    hasher.update([0]);
    hasher.update(format!("mt={limit}").as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}
