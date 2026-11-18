//! 请求内建会话再开流；身份变化不沿用旧会话，只有明确401且尚无输出可重试。

use super::{
    account,
    credentials::{self, Credentials},
    errors, models,
    prompt::{self, Prompt},
    stream::Output,
    upstream, LIGHTAI_BASE,
};
use agent2api_wasm_sdk::{self as sdk, http, json, Result, Value};

const ASSISTANT_QUERY: &str = "name=orcaterm&mode=0&csrfCode=&sseresume=true";

pub fn forward(params: &Value) -> Result<()> {
    let body = &params["body"];
    let mut credentials = account::ensure()?;
    let requested = body["model"].as_str().unwrap_or("");
    let model = models::selected(requested)?;
    let prompt = prompt::build(body, &credentials, model["supportsImages"] == true)?;
    account::validate_current(&credentials)?;
    let conversation = match create_conversation(&credentials) {
        Ok(conversation) => conversation,
        Err(error) if errors::is_auth(&error) => {
            credentials = account::renew(&credentials, true)?;
            create_conversation(&credentials)?
        }
        Err(error) => return Err(error),
    };
    let response = match open_stream(&credentials, &conversation, &model, &prompt, body) {
        Ok(response) => response,
        Err(error) if errors::is_auth(&error) => {
            credentials = account::renew(&credentials, true)?;
            // 同一账号换 token 后仅重开尚未输出的这一步，不重建或拼接另一账号回答。
            open_stream(&credentials, &conversation, &model, &prompt, body)?
        }
        Err(error) => return Err(error),
    };
    Output::new(requested).forward(response, &credentials)
}

fn create_conversation(credentials: &Credentials) -> Result<String> {
    account::validate_current(credentials)?;
    let uuid = credentials::random_uuid()?;
    let body = json!({ "id": uuid, "title": "新对话", "ts": sdk::now(),
        "customTitle": "", "aiTitle": "", "tags": ["orcaterm"] });
    let value = upstream::post_json(
        &format!("{LIGHTAI_BASE}/assistant/conversation"),
        upstream::assistant_headers(credentials, &credentials::random_uuid()?, false),
        &body,
        "OrcaTerm 建会话",
        &[&credentials.access_token],
    )?;
    if errors::business_failed(&value) {
        return Err(errors::upstream(200, &value, &[&credentials.access_token]));
    }
    account::validate_current(credentials)?;
    Ok(format!("cid-{uuid}-orcaterm"))
}

fn open_stream(
    credentials: &Credentials,
    conversation: &str,
    model: &Value,
    prompt: &Prompt,
    request: &Value,
) -> Result<http::Response> {
    account::validate_current(credentials)?;
    let seq_id = credentials::random_uuid()?;
    let wire = model["orcatermNative"]["model"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| format!("TokenHub/{}", model["id"].as_str().unwrap_or("")));
    let (thinking, reasoning) = models::settings(model, request);
    let mut kwargs = json!({});
    for key in ["temperature", "top_p", "max_tokens", "stop"] {
        if let Some(value) = request.get(key) {
            kwargs[key] = value.clone();
        }
    }
    if let Some(value) = request.get("max_completion_tokens") {
        kwargs["max_tokens"] = value.clone();
    }
    if let Some(fields) = reasoning.as_object() {
        for (key, value) in fields {
            kwargs[key] = value.clone();
        }
    }
    let body = json!({
        "conversationId": conversation,
        "user": { "id": credentials.user_id, "setting": {
            "model": wire, "modelDesc": { "kwargs": kwargs }, "enableThinking": thinking,
            "multimodal": !prompt.files.is_empty() || prompt.history.iter().any(|message| message["files"].as_array().is_some_and(|files| !files.is_empty())),
            "mcpServers": [], "uiServers": [], "tools": [], "approvedTools": [], "approvedMCPTools": [],
            "selectedMCPTools": {}, "selectedUITools": {}, "enableAutoSubtaskExecution": false,
            "env": [], "hideToolMessage": true, "language": "zh"
        } },
        "input": { "type": "start", "input": prompt.input, "id": seq_id,
            "historyMessages": prompt.history, "emphasisPrompt": prompt.emphasis, "files": prompt.files },
        "stream": true
    });
    upstream::open(
        &format!("{LIGHTAI_BASE}/assistant/chat?{ASSISTANT_QUERY}"),
        upstream::assistant_headers(credentials, &seq_id, true),
        &body,
        "OrcaTerm 对话",
        &[&credentials.access_token],
    )
}
