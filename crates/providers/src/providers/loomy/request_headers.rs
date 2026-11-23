//! Loomy 推理请求的追踪上下文；Chat / 识图 / Images 各有用途头。
//!
//! 官方客户端（0.9.39）约定：
//! - 纯文本对话 → `X-Loomy-Request-Purpose: chat.message`
//! - 识图 / 多模态图文 → `kb.vision`（另带 `X-Loomy-Billing-Idempotency-Key`）
//! - 生图 → `image.generate`
//!
//! 实测：带 `image_url` 仍发 `chat.message` 时，上游会注入「视觉分析跳过」并
//! 忽略图片；只有 `kb.vision` 才会真正识图。

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};
use serde_json::Value;

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

/// OpenAI / Anthropic 内容块里有没有图片。
pub(super) fn body_has_images(body: &Value) -> bool {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        // Anthropic Messages 也可能在顶层 content（本家 build_chat 先看 chat 体）
        return content_has_images(body.get("content"));
    };
    messages.iter().any(|message| content_has_images(message.get("content")))
}

fn content_has_images(content: Option<&Value>) -> bool {
    match content {
        Some(Value::Array(parts)) => parts.iter().any(part_is_image),
        _ => false,
    }
}

fn part_is_image(part: &Value) -> bool {
    match part.get("type").and_then(Value::as_str).unwrap_or("") {
        "image_url" | "input_image" | "image" => true,
        _ => part.pointer("/image_url/url").and_then(Value::as_str).is_some()
            || part
                .get("source")
                .and_then(|source| source.get("type"))
                .and_then(Value::as_str)
                == Some("base64"),
    }
}

/// 官方 `kb-vision:<sha256>`；无稳定路径时对图片 URL 做指纹。
fn vision_billing_key(body: &Value) -> String {
    let mut hasher = Sha256::new();
    let mut found = false;
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for message in messages {
            if let Some(parts) = message.get("content").and_then(Value::as_array) {
                for part in parts {
                    if let Some(url) = image_url_of(part) {
                        hasher.update(url.as_bytes());
                        hasher.update(b"\0");
                        found = true;
                    }
                }
            }
        }
    }
    if !found {
        hasher.update(body.to_string().as_bytes());
    }
    format!("kb-vision:{:x}", hasher.finalize())
}

fn image_url_of(part: &Value) -> Option<&str> {
    part.pointer("/image_url/url")
        .and_then(Value::as_str)
        .or_else(|| part.get("image_url").and_then(Value::as_str))
        .or_else(|| part.pointer("/source/data").and_then(Value::as_str))
        .map(str::trim)
        .filter(|url| !url.is_empty())
}

pub(super) fn chat(
    session: &str,
    client_headers: &BTreeMap<String, String>,
    body: &Value,
) -> Vec<(String, String)> {
    let vision = body_has_images(body);
    let purpose = if vision { "kb.vision" } else { "chat.message" };
    let mut headers = tracking_headers(session, client_headers, purpose);
    headers.push(("Accept".to_string(), "text/event-stream".to_string()));
    headers.push(("loomy-version".to_string(), endpoints::client_version()));
    if vision {
        // 官方资料库识图必带；缺它上游也可能放行，但计费幂等与客户端对齐。
        headers.push((
            "X-Loomy-Billing-Idempotency-Key".to_string(),
            vision_billing_key(body),
        ));
    }
    headers
}

pub(super) fn images(
    session: &str,
    client_headers: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut headers = tracking_headers(session, client_headers, "image.generate");
    headers.push(("Accept".to_string(), "application/json".to_string()));
    headers.push(("loomy-version".to_string(), endpoints::client_version()));
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_openai_image_url_parts() {
        let body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "看图"},
                    {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,aaa"}}
                ]
            }]
        });
        assert!(body_has_images(&body));
        assert!(!body_has_images(&json!({
            "messages": [{"role": "user", "content": "你好"}]
        })));
    }

    #[test]
    fn vision_purpose_when_images_present() {
        let body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image_url", "image_url": {"url": "https://example.com/a.jpg"}}
                ]
            }]
        });
        let headers = chat("sess", &BTreeMap::new(), &body);
        let purpose = headers
            .iter()
            .find(|(k, _)| k == "X-Loomy-Request-Purpose")
            .map(|(_, v)| v.as_str());
        assert_eq!(purpose, Some("kb.vision"));
        assert!(headers.iter().any(|(k, v)| {
            k == "X-Loomy-Billing-Idempotency-Key" && v.starts_with("kb-vision:")
        }));
        assert!(headers.iter().any(|(k, _)| k == "loomy-version"));
    }

    #[test]
    fn chat_message_purpose_for_text_only() {
        let body = json!({
            "messages": [{"role": "user", "content": "你好"}]
        });
        let headers = chat("sess", &BTreeMap::new(), &body);
        let purpose = headers
            .iter()
            .find(|(k, _)| k == "X-Loomy-Request-Purpose")
            .map(|(_, v)| v.as_str());
        assert_eq!(purpose, Some("chat.message"));
        assert!(!headers
            .iter()
            .any(|(k, _)| k == "X-Loomy-Billing-Idempotency-Key"));
    }
}
