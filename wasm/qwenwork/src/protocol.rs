//! 千问办公 qoder_work/processing 请求信封，与 COSY 六号客户端、agree、qwork 一组对应。
//! model_config 保留千问宽形状、剥离 thinking_config；两处上下文长度必须与当前目录一致。
use super::{context, credentials::Credentials, endpoints, messages, reasoning, util};
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
    for field in ["max_tokens", "max_completion_tokens"] {
        if body
            .get(field)
            .is_some_and(|value| !value.is_null() && value.as_u64().is_none_or(|value| value == 0))
        {
            return Err(Error::http(400, format!("千问办公 {field} 必须为正整数")));
        }
    }
    if body
        .get("n")
        .is_some_and(|value| !value.is_null() && value.as_u64() != Some(1))
    {
        return Err(Error::http(400, "千问办公每次请求只返回一条回答"));
    }
    if body
        .get("parallel_tool_calls")
        .is_some_and(|value| !value.is_null() && !value.is_boolean())
    {
        return Err(Error::http(
            400,
            "千问办公 parallel_tool_calls 必须为布尔值",
        ));
    }
    let limit = body["max_completion_tokens"]
        .as_u64()
        .or_else(|| body["max_tokens"].as_u64())
        .unwrap_or(endpoints::MAX_OUTPUT as u64)
        .min(endpoints::MAX_OUTPUT as u64) as i64;
    let record = record_id(key, &messages, &tools, limit);
    let seed = session
        .filter(|session| !session.is_empty())
        .or_else(|| body["prompt_cache_key"].as_str())
        .or_else(|| body["session_id"].as_str())
        .or_else(|| body["user"].as_str());
    let session = session_id(&credentials.binding(), key, seed)?;
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
    for field in [
        "temperature",
        "top_p",
        "presence_penalty",
        "frequency_penalty",
    ] {
        if let Some(value) = body.get(field).filter(|value| !value.is_null()) {
            let number = value
                .as_f64()
                .filter(|value| value.is_finite())
                .ok_or_else(|| Error::http(400, format!("千问办公 {field} 必须为有效数值")))?;
            if field == "temperature" && !(0.0..=2.0).contains(&number)
                || field == "top_p" && !(0.0..=1.0).contains(&number)
                || ["presence_penalty", "frequency_penalty"].contains(&field)
                    && !(-2.0..=2.0).contains(&number)
            {
                return Err(Error::http(400, format!("千问办公 {field} 超出支持范围")));
            }
            parameters[field] = value.clone();
        }
    }
    for field in ["stop", "seed", "response_format"] {
        if let Some(value) = body.get(field).filter(|value| !value.is_null()) {
            parameters[field] = value.clone();
        }
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
    if !tools.is_empty() {
        if let Some(choice) = body.get("tool_choice").filter(|value| !value.is_null()) {
            payload["tool_choice"] = choice.clone();
        }
        if let Some(parallel) = body
            .get("parallel_tool_calls")
            .filter(|value| !value.is_null())
        {
            payload["parallel_tool_calls"] = parallel.clone();
        }
    }
    context::apply(&mut payload, config, &messages, &tools);
    Ok(Payload {
        value: payload,
        thinking: util::truthy(&model["reasoning"])
            || thinking.enabled.is_some()
            || thinking.effort.is_some(),
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
        Some(value) => util::hash(value),
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
