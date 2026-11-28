//! 沿用 Loomy ChatId/MsgId/TurnId/traceparent 及用途头，不转发客户端鉴权头。

use agent2api_wasm_sdk::{json, Result, Value};
use sha2::{Digest, Sha256};

use super::{sign, CLIENT_VERSION};

fn header(headers: &Value, name: &str) -> Option<String> {
    let value = headers
        .as_object()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))?
        .1
        .as_str()?
        .trim();
    (!value.is_empty()
        && value.len() <= 256
        && value.is_ascii()
        && !value.bytes().any(|byte| byte.is_ascii_control()))
    .then(|| value.to_string())
}

fn valid_traceparent(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    let hex = |value: &str, length: usize| {
        value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    };
    parts.len() == 4
        && parts[0] == "00"
        && hex(parts[1], 32)
        && hex(parts[2], 16)
        && hex(parts[3], 2)
        && parts[1].bytes().any(|byte| byte != b'0')
        && parts[2].bytes().any(|byte| byte != b'0')
}

pub fn build(session: &str, incoming: &Value, image: bool) -> Result<Value> {
    build_with_session(session, incoming, image, "")
}

fn session_digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

pub fn build_with_session(
    session: &str,
    incoming: &Value,
    image: bool,
    conversation: &str,
) -> Result<Value> {
    let affinity = header(incoming, "x-session-affinity");
    let conversation = header(incoming, "x-opencode-session")
        .or_else(|| header(incoming, "x-session-id"))
        .or_else(|| affinity.clone())
        .or_else(|| {
            let value = conversation.trim();
            (!value.is_empty() && value.len() <= 1024 && !value.contains('\0'))
                .then(|| value.to_string())
        });
    let chat_id = match header(incoming, "chatid") {
        Some(value) => value,
        None => match conversation {
            Some(value) => {
                let digest = session_digest(&value);
                format!("chat-{}-{}-{}-{}-{}", &digest[..8], &digest[8..12],
                    &digest[12..16], &digest[16..20], &digest[20..32])
            }
            None => format!("chat-{}", sign::uuid()?),
        },
    };
    let msg_id = match header(incoming, "msgid").or_else(|| header(incoming, "x-request-id")) {
        Some(value) => value,
        None => format!("msg-{}", sign::uuid()?),
    };
    let turn_id = header(incoming, "turnid").unwrap_or_else(|| msg_id.clone());
    let traceparent = match header(incoming, "traceparent").filter(|value| valid_traceparent(value))
    {
        Some(value) => value.to_ascii_lowercase(),
        None => format!("00-{}-{}-01", sign::random_hex(16)?, sign::random_hex(8)?),
    };
    let affinity = affinity.unwrap_or_else(|| format!("ses_{}", &session_digest(&chat_id)[..24]));
    let origin = header(incoming, "InvokeOrigin")
        .filter(|value| matches!(value.as_str(), "user" | "system"))
        .unwrap_or_else(|| "user".to_string());
    let purpose = header(incoming, "X-Loomy-Request-Purpose")
        .filter(|value| matches!(value.as_str(),
            "chat.message" | "chat.title" | "pet.comment" | "pet.reward" |
            "kb.vision" | "kb.audio" | "kb.summary" | "kb.embed" | "search.web" |
            "image.generate" | "assistant.completion" | "assistant.asr-correct" |
            "memory.extract" | "memory.recall" | "memory.dream" |
            "skill.evolution" | "skill.curator"))
        .unwrap_or_else(|| if image { "image.generate" } else { "chat.message" }.to_string());
    let mut headers = json!({
        "Content-Type": "application/json;charset=utf-8", "token": session, "Authorization": format!("Bearer {session}"),
        "traceparent": traceparent, "ChatId": chat_id, "MsgId": msg_id, "TurnId": turn_id,
        "x-session-affinity": affinity,
        "InvokeOrigin": origin, "X-Loomy-Request-Purpose": purpose,
        "Accept": if image { "application/json" } else { "text/event-stream" }
    });
    if !image {
        headers["loomy-version"] = json!(CLIENT_VERSION);
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn incoming() -> Value {
        json!({"MsgId": "msg-test", "traceparent": "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01"})
    }

    #[test]
    fn conversation_and_affinity_survive_message_changes() {
        let first = build_with_session("test-session", &incoming(), false, "conversation-1").unwrap();
        let mut next = incoming();
        next["MsgId"] = json!("msg-next");
        let next = build_with_session("test-session", &next, false, "conversation-1").unwrap();
        assert_eq!(first["ChatId"], next["ChatId"]);
        assert_eq!(first["x-session-affinity"], next["x-session-affinity"]);
        assert_ne!(first["MsgId"], next["MsgId"]);
        assert_eq!(next["TurnId"], "msg-next");
        assert!(first["ChatId"].as_str().unwrap().starts_with("chat-"));
        assert!(first["x-session-affinity"].as_str().unwrap().starts_with("ses_"));
        let other = build_with_session("test-session", &incoming(), false, "conversation-2").unwrap();
        assert_ne!(first["ChatId"], other["ChatId"]);
        assert_ne!(first["x-session-affinity"], other["x-session-affinity"]);
    }

    #[test]
    fn preserves_official_tracking_and_uses_account_authentication() {
        let mut input = incoming();
        input["cHaTiD"] = json!("chat-parent");
        input["TurnId"] = json!("msg-parent");
        input["X-Session-Affinity"] = json!("ses_official");
        input["InvokeOrigin"] = json!("system");
        input["X-Loomy-Request-Purpose"] = json!("chat.title");
        input["Authorization"] = json!("Bearer downstream-key");
        let headers = build("test-session", &input, false).unwrap();
        assert_eq!(headers["ChatId"], "chat-parent");
        assert_eq!(headers["TurnId"], "msg-parent");
        assert_eq!(headers["x-session-affinity"], "ses_official");
        assert_eq!(headers["InvokeOrigin"], "system");
        assert_eq!(headers["X-Loomy-Request-Purpose"], "chat.title");
        assert_eq!(headers["Authorization"], "Bearer test-session");
        assert_eq!(headers["token"], "test-session");
    }

    #[test]
    fn supports_opencode_and_affinity_only_clients() {
        for name in ["x-opencode-session", "x-session-id", "x-session-affinity"] {
            let mut input = incoming();
            input[name] = json!("ses_client");
            let first = build("test-session", &input, false).unwrap();
            let next = build("test-session", &input, false).unwrap();
            assert_eq!(first["ChatId"], next["ChatId"]);
            assert_eq!(first["x-session-affinity"], next["x-session-affinity"]);
        }
    }

    #[test]
    fn invalid_business_headers_use_defaults() {
        let mut input = incoming();
        input["ChatId"] = json!("chat-test");
        input["TurnId"] = json!("bad\r\nheader");
        input["InvokeOrigin"] = json!("admin");
        input["X-Loomy-Request-Purpose"] = json!("billing.exempt");
        input["X-Session-Affinity"] = json!("bad\nheader");
        let headers = build("test-session", &input, false).unwrap();
        assert_eq!(headers["TurnId"], "msg-test");
        assert_eq!(headers["InvokeOrigin"], "user");
        assert_eq!(headers["X-Loomy-Request-Purpose"], "chat.message");
        assert!(headers["x-session-affinity"].as_str().unwrap().starts_with("ses_"));
        let image = build("test-session", &input, true).unwrap();
        assert_eq!(image["X-Loomy-Request-Purpose"], "image.generate");
    }
}
