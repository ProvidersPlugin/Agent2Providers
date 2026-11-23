//! 保留原生 Kiro 的额度/认证/上下文错误判据；错误必须先分类再脱敏。
//! 超长输入和模型容量不足不能因为 exceeded/insufficient 字样被误罚成账号限额。

use crate::util;
use agent2api_wasm_sdk::{json, Error, Value};

pub const TOO_LONG: &str = "input is too long for the model's context: ";

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Auth,
    Quota,
    Content,
    Other,
}

pub fn too_long(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "content_length_exceeds_threshold",
        "model_context_window_exceeded",
        "input is too long",
    ]
    .iter()
    .any(|hint| lower.contains(hint))
}
pub fn kind(status: u16, message: &str) -> Kind {
    let lower = message.to_ascii_lowercase();
    if too_long(message) || lower.contains("insufficient_model_capacity") {
        return Kind::Other;
    }
    if [
        "content_filtered",
        "content_filter",
        "content blocked",
        "blocked by content policy",
    ]
    .iter()
    .any(|hint| lower.contains(hint))
    {
        return Kind::Content;
    }
    if ["monthly_request_count", "usage_limit"]
        .iter()
        .any(|hint| lower.contains(hint))
    {
        return Kind::Quota;
    }
    if matches!(status, 401 | 403)
        || [
            "expiredtoken",
            "expired token",
            "tokenexpired",
            "unauthorizedexception",
            "invalid_token",
            "invalid token",
            "invalid bearer token",
            "accessdeniedexception",
        ]
        .iter()
        .any(|hint| lower.contains(hint))
    {
        return Kind::Auth;
    }
    if status == 429
        || [
            "quota",
            "insufficient",
            "balance",
            "credit",
            "billing",
            "usage limit",
            "rate limit",
            "too many requests",
            "throttl",
            "limit reached",
            "overloaded",
            "额度",
            "限流",
            "频率",
        ]
        .iter()
        .any(|hint| lower.contains(hint))
    {
        return Kind::Quota;
    }
    Kind::Other
}
pub fn message(value: &Value) -> String {
    let own = if let Some(text) = value.as_str() {
        text.to_string()
    } else {
        util::text(value, &["message", "Message", "error_description"])
    };
    let own = if own.is_empty() {
        if let Some(text) = value.get("error").and_then(Value::as_str) {
            text.to_string()
        } else {
            util::text(&value["error"], &["message", "Message"])
        }
    } else {
        own
    };
    let reason = util::text(value, &["reason", "__type", "code"]);
    if reason.is_empty() {
        own
    } else {
        format!("{own} ({reason})")
    }
}
pub fn upstream(status: u16, value: &Value, secrets: &[&str]) -> Error {
    let scalar = value.is_string();
    let message = message(value);
    let capacity = message
        .to_ascii_lowercase()
        .contains("insufficient_model_capacity");
    let (effective, code) = if too_long(&message) {
        (400, "context_length_exceeded")
    } else if capacity {
        (503, "kiro_model_capacity")
    } else {
        match kind(status, &message) {
            Kind::Auth => (401, "kiro_auth"),
            Kind::Quota => (429, "kiro_quota"),
            Kind::Content => (400, "kiro_content_blocked"),
            Kind::Other => (
                if (400..600).contains(&status) {
                    status
                } else {
                    502
                },
                "kiro_upstream",
            ),
        }
    };
    let safe = if scalar {
        String::new()
    } else {
        safe_message(&message, secrets)
    };
    let safe = if safe.is_empty() {
        format!("Kiro 请求失败（HTTP {effective}）")
    } else {
        safe
    };
    let safe = if too_long(&message) && !safe.starts_with(TOO_LONG) {
        format!("{TOO_LONG}{safe}")
    } else {
        safe
    };
    Error::http(effective, safe)
        .with_code(code)
        .with_reset(util::timestamp(
            value.get("reset_at").or_else(|| value.get("resetAt")),
        ))
}
pub fn exception(event: &str, value: &Value, secrets: &[&str]) -> Error {
    if value.is_string() {
        let text = format!("{event}: {}", message(value));
        return upstream(502, &json!(text), secrets);
    }
    upstream(
        502,
        &json!({ "message":format!("{event}: {}", message(value)),
        "reset_at":value["reset_at"], "resetAt":value["resetAt"] }),
        secrets,
    )
}
pub fn classify(status: u16, body: &Value) -> Value {
    let text = message(body);
    let code = body["code"]
        .as_str()
        .or_else(|| body["error"]["code"].as_str())
        .unwrap_or("");
    if code == "account_model_unavailable" {
        return json!({ "kind":"fatal", "status":400, "message":safe_message(&text, &[]),
            "reset_at":null, "upstream_code":null });
    }
    let (name, effective) = if code == "context_length_exceeded" || too_long(&text) {
        ("fatal", 400)
    } else if code == "kiro_model_capacity"
        || text
            .to_ascii_lowercase()
            .contains("insufficient_model_capacity")
    {
        ("fatal", 503)
    } else if code == "kiro_content_blocked" {
        ("content_blocked", 400)
    } else {
        match kind(status, &text) {
            Kind::Auth => ("token_expired", 401),
            Kind::Quota => ("quota_limited", 429),
            Kind::Content => ("content_blocked", 400),
            Kind::Other => ("fatal", status.max(400)),
        }
    };
    json!({ "kind": name, "status": effective, "message": safe_message(&text, &[]),
        "reset_at": util::timestamp(body.get("reset_at").or_else(|| body.get("resetAt"))
            .or_else(|| body["error"].get("reset_at")).or_else(|| body["error"].get("resetAt"))),
        "upstream_code": body["upstream_code"].as_i64().or_else(|| body["error"]["upstream_code"].as_i64()) })
}
pub fn host_stop(error: &Error) -> bool {
    matches!(
        error.code.as_deref(),
        Some(
            "aborted"
                | "cancelled"
                | "runtime_stream_interrupted"
                | "plugin_unavailable"
                | "forbidden"
                | "plugin_resource_limit"
                | "plugin_protocol_error"
                | "native_helper_not_authorized"
        )
    )
}
pub fn transport(error: Error) -> Error {
    if host_stop(&error) {
        return error;
    }
    if error.code.as_deref() == Some("timeout") {
        Error::http(504, "Kiro 请求超时，请检查网络与账号代理").with_code("kiro_transport")
    } else {
        Error::http(502, "无法连接 Kiro，请检查网络与账号代理").with_code("kiro_transport")
    }
}
pub fn redact(input: &str, secrets: &[&str]) -> String {
    let mut text = input.to_string();
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        for needle in [secret.to_string(), util::query(secret)] {
            text = text.replace(&needle, "[已隐藏]");
        }
    }
    text
}
pub fn safe_message(input: &str, secrets: &[&str]) -> String {
    let text = redact(input, secrets);
    let lower = text.to_ascii_lowercase();
    // 错误不回显任意 URL、授权码或客户端秘密；完整报文只交宿主脱敏调试设施。
    if [
        "http://",
        "https://",
        "code=",
        "code_verifier",
        "codeverifier",
        "clientsecret",
        "client_secret",
        "access_token",
        "accesstoken",
        "refresh_token",
        "refreshtoken",
        "bearer ",
        "ksk_",
    ]
    .iter()
    .any(|hint| lower.contains(hint))
    {
        return "Kiro 拒绝了本次请求，详细凭据信息已隐藏".into();
    }
    text.chars()
        .filter(|ch| !ch.is_control())
        .take(500)
        .collect()
}
pub fn sanitize(value: &Value, secrets: &[&str]) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| {
                    let key = key.to_ascii_lowercase().replace(['_', '-'], "");
                    ![
                        "access",
                        "refresh",
                        "accesstoken",
                        "refreshtoken",
                        "clientsecret",
                        "authorization",
                        "apikey",
                        "token",
                        "password",
                        "cookie",
                        "codeverifier",
                    ]
                    .contains(&key.as_str())
                })
                .map(|(key, value)| (key.clone(), sanitize(value, secrets)))
                .collect(),
        ),
        Value::Array(items) => {
            Value::Array(items.iter().map(|value| sanitize(value, secrets)).collect())
        }
        Value::String(text) => json!(redact(text, secrets)),
        _ => value.clone(),
    }
}
pub fn event(error: &Error) -> Value {
    json!({ "error": { "message": error.message, "status": error.status.unwrap_or(502), "code": error.code,
        "reset_at":error.reset_at, "upstream_code":error.upstream_code,
        "type": if error.status == Some(429) { "rate_limit_error" } else { "upstream_error" } } })
}
