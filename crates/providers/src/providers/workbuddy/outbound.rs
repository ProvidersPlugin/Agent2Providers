//! 对话 URL、请求头与会话折叠在所有 WorkBuddy 链路共用。

use super::{auth, region::Region};
use crate::ids::{new_conversation_id, new_message_id, new_span_id};
use crate::workbuddy_shared::endpoints::normalize_endpoint;
use serde_json::Value;

pub(super) const DEFAULT_SYSTEM_PROMPT: &str = "你是一个得力助手";

pub fn ensure_leading_system_message(body: &Value) -> Option<Value> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .filter(|messages| !messages.is_empty())?;
    let role = messages[0]
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("");
    if role.eq_ignore_ascii_case("system") {
        return None;
    }
    let mut next = body.clone();
    let fields = next.as_object_mut()?;
    let mut with_system = Vec::with_capacity(messages.len() + 1);
    with_system.push(serde_json::json!({ "role": "system", "content": DEFAULT_SYSTEM_PROMPT }));
    with_system.extend(messages.iter().cloned());
    fields.insert("messages".into(), Value::Array(with_system));
    Some(next)
}

pub(crate) fn session_of_record(record: &Value, region: Region) -> Value {
    let mut account = serde_json::Map::new();
    for key in [
        "uid",
        "enterpriseId",
        "type",
        "departmentFullName",
        "name",
        "nickname",
        "oneidAccountId",
        "idp",
    ] {
        if let Some(value) = record.get(key) {
            account.insert(key.into(), value.clone());
        }
    }
    let mut auth = serde_json::Map::new();
    for key in [
        "accessToken",
        "refreshToken",
        "expiresAt",
        "refreshExpiresAt",
        "domain",
        "tokenType",
    ] {
        if let Some(value) = record.get(key) {
            auth.insert(key.into(), value.clone());
        }
    }
    let mut session = serde_json::Map::new();
    session.insert("account".into(), Value::Object(account));
    session.insert("auth".into(), Value::Object(auth));
    session.insert("edition".into(), Value::String(region.id().into()));
    for key in ["id", "endpoint", "proxy", "prefixPath", "platform"] {
        if let Some(value) = record.get(key) {
            session.insert(key.into(), value.clone());
        }
    }
    Value::Object(session)
}

pub(crate) fn chat_headers(
    session: &Value,
    region: Region,
    accept: Option<&str>,
) -> Vec<(String, String)> {
    let edition = region.edition();
    let request_id = new_message_id();
    let message_id = new_message_id();
    let trace_id = new_message_id();
    let span_id = new_span_id();
    let mut headers = vec![
        ("Content-Type".into(), "application/json".into()),
        ("User-Agent".into(), edition.chat_ua.into()),
        ("X-IDE-Type".into(), edition.ua_platform.into()),
        ("X-IDE-Name".into(), edition.ua_platform.into()),
        ("X-IDE-Version".into(), edition.chat_ide_version.into()),
        ("X-Product".into(), "SaaS".into()),
        ("X-Agent-Intent".into(), "craft".into()),
        ("X-Agent-Purpose".into(), "conversation".into()),
        ("X-Agent-Type".into(), "main".into()),
        ("X-Private-Data".into(), "true".into()),
        ("X-Conversation-ID".into(), new_conversation_id()),
        ("X-Conversation-Request-ID".into(), request_id.clone()),
        ("X-Root-Request-ID".into(), request_id),
        ("X-Conversation-Message-ID".into(), message_id.clone()),
        ("X-Request-ID".into(), message_id),
        ("X-Trace-ID".into(), trace_id.clone()),
        ("traceparent".into(), format!("00-{trace_id}-{span_id}-01")),
        ("b3".into(), format!("{trace_id}-{span_id}-1")),
        ("X-B3-TraceId".into(), trace_id),
        ("X-B3-SpanId".into(), span_id),
        ("X-B3-Sampled".into(), "1".into()),
        ("X-Requested-With".into(), "XMLHttpRequest".into()),
    ];
    if region == Region::Cn {
        headers.push(("X-Agent-Runtime".into(), "local-agent".into()));
    }
    headers.extend(auth::build_auth_headers(session));
    if region == Region::Intl {
        headers.retain(|(name, _)| !name.eq_ignore_ascii_case("X-Domain"));
        headers.push(("X-Domain".into(), "www.workbuddy.ai".into()));
    }
    if let Some(accept) = accept {
        headers.push(("Accept".into(), accept.into()));
    }
    headers
}

pub(crate) fn session_endpoint_of(session: &Value, region: Region) -> String {
    session
        .get("endpoint")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|endpoint| !endpoint.is_empty())
        .map(normalize_endpoint)
        .unwrap_or_else(|| {
            normalize_endpoint(
                &region
                    .env_endpoint_override()
                    .unwrap_or_else(|| region.default_endpoint().into()),
            )
        })
}

pub(crate) fn chat_completions_url(session: &Value, region: Region) -> String {
    format!(
        "{}/v2/chat/completions",
        session_endpoint_of(session, region)
    )
}
