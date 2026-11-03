//! Loomy 推理请求的追踪上下文；Chat 使用客户端版本头，Images 使用生图用途。

use std::collections::BTreeMap;

use crate::headers::Headers;
use crate::ids;

use super::endpoints;

const MAX_ID_LENGTH: usize = 256;

fn header(headers: &Headers, name: &str) -> Option<String> {
    let value = headers.get(name)?.trim();
    (!value.is_empty()
        && value.len() <= MAX_ID_LENGTH
        && value.is_ascii()
        && !value.bytes().any(|byte| byte.is_ascii_control()))
    .then(|| value.to_string())
}

fn valid_traceparent(value: &str) -> bool {
    let mut parts = value.split('-');
    let (Some(version), Some(trace), Some(span), Some(flags)) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let hex = |part: &str, length: usize| {
        part.len() == length && part.bytes().all(|byte| byte.is_ascii_hexdigit())
    };
    version == "00"
        && hex(trace, 32)
        && hex(span, 16)
        && hex(flags, 2)
        && trace.bytes().any(|byte| byte != b'0')
        && span.bytes().any(|byte| byte != b'0')
        && parts.next().is_none()
}

fn tracking_headers(
    session: &str,
    client_headers: &BTreeMap<String, String>,
    purpose: &str,
) -> Vec<(String, String)> {
    let incoming = Headers::from_map(client_headers);
    let chat_id = header(&incoming, "chatid")
        .or_else(|| header(&incoming, "x-session-id"))
        .unwrap_or_else(ids::uuid_v4);
    let message_id = header(&incoming, "msgid")
        .or_else(|| header(&incoming, "x-request-id"))
        .unwrap_or_else(ids::uuid_v4);
    let turn_id = header(&incoming, "turnid").unwrap_or_else(|| message_id.clone());
    let traceparent = header(&incoming, "traceparent")
        .filter(|value| valid_traceparent(value))
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_else(|| format!("00-{}-{}-01", ids::new_message_id(), ids::new_span_id()));

    vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("token".to_string(), session.to_string()),
        ("Authorization".to_string(), format!("Bearer {session}")),
        ("traceparent".to_string(), traceparent),
        ("ChatId".to_string(), chat_id),
        ("MsgId".to_string(), message_id),
        ("TurnId".to_string(), turn_id),
        ("InvokeOrigin".to_string(), "user".to_string()),
        ("X-Loomy-Request-Purpose".to_string(), purpose.to_string()),
    ]
}

pub(super) fn chat(
    session: &str,
    client_headers: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut headers = tracking_headers(session, client_headers, "chat.message");
    headers.push(("Accept".to_string(), "text/event-stream".to_string()));
    headers.push(("loomy-version".to_string(), endpoints::client_version()));
    headers
}

pub(super) fn images(
    session: &str,
    client_headers: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut headers = tracking_headers(session, client_headers, "image.generate");
    headers.push(("Accept".to_string(), "application/json".to_string()));
    headers
}
