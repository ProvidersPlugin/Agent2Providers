//! Start Plan 请求的会话归因；请求头与 metadata 必须使用同一个会话标识。

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::credentials;

const SESSION_PREFIXES: [&str; 2] = ["sess_", "subagent_agent_"];
const SESSION_HEADERS: [&str; 7] = [
    "x-opencode-session",
    "x-claude-code-session-id",
    "claude-code-session-id",
    "x-vscode-sessionid",
    "x-session-id",
    "x-parent-session-id",
    "helicone-session-id",
];

pub(super) struct RequestContext {
    request_id: Option<String>,
    trace_id: Option<String>,
    query_id: Option<String>,
    session_id: Option<String>,
    session_type: &'static str,
}

impl RequestContext {
    pub fn new(headers: &BTreeMap<String, String>, body: &Value, custom_system: bool) -> Self {
        let metadata = body.get("metadata");
        let user_id = metadata
            .and_then(|value| value.get("user_id"))
            .and_then(Value::as_str)
            .and_then(|text| serde_json::from_str::<Value>(text).ok());
        let session_id = header(headers, &SESSION_HEADERS)
            .or_else(|| field(metadata, &["sessionId", "session_id", "conversationId", "conversation_id"]))
            .or_else(|| field(user_id.as_ref(), &["session_id"]))
            .or_else(credentials::new_uuid);
        // 自定义 system 对应 ContextBuilder 的子代理路径，不能与 main 身份混用。
        let session_type = if custom_system {
            "subagent"
        } else {
            match header(headers, &["x-zcode-session-type"]).as_deref() {
                Some("subagent") => "subagent",
                Some("side_chat") => "side_chat",
                Some("other") => "other",
                Some("main") => "main",
                _ if session_id.as_deref().is_some_and(|id| id.starts_with("subagent_agent_")) => {
                    "subagent"
                }
                _ => "main",
            }
        };
        Self {
            request_id: header(headers, &["x-request-id", "x-client-request-id"])
                .or_else(|| field(metadata, &["requestId", "request_id"]))
                .or_else(credentials::new_uuid),
            trace_id: header(headers, &["x-zcode-trace-id"])
                .or_else(|| field(metadata, &["traceId", "trace_id"]))
                .or_else(credentials::new_uuid),
            query_id: header(headers, &["x-query-id"])
                .or_else(|| field(metadata, &["queryId", "query_id"]))
                .map(|id| strip_prefixes(&id, &["query_"]).to_string()),
            session_id: session_id.map(|id| strip_prefixes(&id, &SESSION_PREFIXES).to_string()),
            session_type,
        }
    }

    pub fn headers(&self) -> Vec<(String, String)> {
        let mut headers = Vec::new();
        if let Some(id) = &self.request_id {
            headers.push(("x-request-id".to_string(), id.clone()));
        }
        headers.push(("x-zcode-session-type".to_string(), self.session_type.to_string()));
        if let Some(id) = &self.trace_id {
            headers.push(("x-zcode-trace-id".to_string(), id.clone()));
        }
        if let Some(id) = &self.query_id {
            headers.push(("x-query-id".to_string(), id.clone()));
        }
        if let Some(id) = &self.session_id {
            headers.push(("x-session-id".to_string(), id.clone()));
        }
        headers
    }

    pub fn metadata_user_id(&self, device_mid: &str) -> Option<String> {
        let mut blob = json!({
            "account_uuid": "",
            "session_id": self.session_id.as_deref().unwrap_or(""),
        });
        if !device_mid.trim().is_empty() {
            blob["device_id"] = Value::String(device_mid.trim().to_string());
        }
        serde_json::to_string(&blob).ok()
    }
}

fn header(headers: &BTreeMap<String, String>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| headers.get(*key).and_then(|value| printable(value)))
}

fn field(value: Option<&Value>, keys: &[&str]) -> Option<String> {
    let value = value?;
    keys.iter().find_map(|key| value.get(*key).and_then(Value::as_str).and_then(printable))
}

fn printable(raw: &str) -> Option<String> {
    let text = raw.trim();
    (!text.is_empty() && text.len() <= 2048 && text.bytes().all(|byte| (32..=126).contains(&byte)))
        .then(|| text.to_string())
}

fn strip_prefixes<'a>(value: &'a str, prefixes: &[&str]) -> &'a str {
    let mut out = value;
    for prefix in prefixes {
        if let Some(rest) = out.strip_prefix(prefix).filter(|rest| !rest.is_empty()) {
            out = rest;
        }
    }
    out
}
