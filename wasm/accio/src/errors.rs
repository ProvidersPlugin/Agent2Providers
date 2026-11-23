//! 保留 Accio 真实错误判据，HTTP 200 业务失败不能落进成功流。

use agent2api_wasm_sdk::{json, Error, Value};

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Auth,
    Quota,
    Content,
    Other,
}

pub fn kind(status: u16, message: &str) -> Kind {
    let lower = message.to_ascii_lowercase();
    if status == 401 {
        return Kind::Auth;
    }
    if status == 429 {
        return Kind::Quota;
    }
    if [
        "unauthorized",
        "not logged in",
        "not_login",
        "invalid token",
        "token expired",
        "invalid_token",
        "invalid_grant",
        "invalid refresh token",
        "refresh token has been used",
        "refresh_token has been used",
        "refresh token expired",
        "refresh_token expired",
    ]
    .iter()
    .any(|hint| lower.contains(hint))
    {
        return Kind::Auth;
    }
    if [
        "积分",
        "余额",
        "额度",
        "quota",
        "insufficient",
        "credit",
        "exceeded",
        "limit reached",
        "rate limit",
        "too many requests",
    ]
    .iter()
    .any(|hint| lower.contains(hint))
    {
        return Kind::Quota;
    }
    if [
        "blocked by security policy",
        "unapproved channel",
        "illegal api invocation",
    ]
    .iter()
    .any(|hint| lower.contains(hint))
    {
        return Kind::Content;
    }
    Kind::Other
}

pub fn number(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse().ok())
}

pub fn message(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.into();
    }
    let top = crate::util::text(value, &["errorMessage", "error_message", "message", "msg"]);
    if !top.is_empty() {
        return top;
    }
    crate::util::text(&value["error"], &["message", "msg"])
}

pub fn code(value: &Value) -> String {
    let own = crate::util::text(value, &["errorCode", "error_code", "code"]);
    if own.is_empty() {
        crate::util::text(&value["error"], &["code"])
    } else {
        own
    }
}

pub fn upstream(status: u16, code: &str, message: &str, secrets: &[&str]) -> Error {
    let numeric = code.parse::<i64>().ok();
    let effective = if (200..300).contains(&status) {
        numeric
            .and_then(|code| u16::try_from(code).ok())
            .filter(|code| (400..600).contains(code))
            .unwrap_or(status)
    } else {
        status
    };
    let raw = format!("{code} {message}");
    let (status, provider_code) = match kind(effective, &raw) {
        Kind::Auth => (401, "accio_auth"),
        Kind::Quota => (429, "accio_quota"),
        Kind::Content => (400, "accio_content_blocked"),
        Kind::Other => (
            if (400..600).contains(&effective) {
                effective
            } else {
                502
            },
            "accio_upstream",
        ),
    };
    let safe: String = redact(message, secrets)
        .chars()
        .filter(|ch| !ch.is_control())
        .take(400)
        .collect();
    let mut error = Error::http(
        status,
        if safe.is_empty() {
            format!("Accio 国际版请求失败（HTTP {status}）")
        } else {
            format!("Accio 国际版请求失败：{safe}")
        },
    )
    .with_code(provider_code);
    error.upstream_code = numeric;
    error
}

pub fn classify(status: u16, body: &Value) -> Value {
    let message = message(body);
    let business_status = number(&body["upstreamCode"])
        .or_else(|| number(&body["upstream_code"]))
        .or_else(|| number(&body["code"]))
        .and_then(|code| u16::try_from(code).ok())
        .filter(|code| (400..600).contains(code));
    let effective = if (200..300).contains(&status) {
        business_status.unwrap_or(status)
    } else {
        status
    };
    let kind = if body["providerCode"] == "accio_content_blocked" {
        Kind::Content
    } else {
        kind(effective, &message)
    };
    let (name, status) = match kind {
        Kind::Auth => ("token_expired", 401),
        Kind::Quota => ("quota_limited", 429),
        Kind::Content => ("content_blocked", 400),
        Kind::Other => (
            "fatal",
            if (400..600).contains(&status) {
                status
            } else {
                502
            },
        ),
    };
    json!({ "kind": name, "status": status, "message": if message.is_empty() { "Accio 国际版上游请求失败" } else { &message },
        "upstream_code": number(&body["upstreamCode"]).or_else(|| number(&body["upstream_code"]))
            .or_else(|| number(&body["code"])),
        "reset_at": number(&body["reset_at"]).or_else(|| number(&body["resetAt"])) })
}

pub fn host_stop(error: &Error) -> bool {
    error.code.as_deref().is_some_and(|code| {
        matches!(
            code,
            "aborted"
                | "plugin_unavailable"
                | "forbidden"
                | "plugin_resource_limit"
                | "plugin_protocol_error"
                | "accio_account_changed"
                | "request_replay_blocked"
        ) || code.starts_with("request_call_")
    })
}

pub fn transport(error: Error) -> Error {
    if host_stop(&error) {
        return error;
    }
    if error.code.as_deref() == Some("timeout") {
        return Error::http(504, "Accio 请求超时，请检查网络与账号代理")
            .with_code("accio_transport");
    }
    Error::http(502, "无法连接 Accio，请检查网络与账号代理").with_code("accio_transport")
}

pub fn redact(input: &str, secrets: &[&str]) -> String {
    let mut output = input.to_string();
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        for needle in [secret.to_string(), crate::util::query(secret)] {
            output = output.replace(&needle, "[已隐藏]");
        }
    }
    output
}

pub fn sanitize(value: &Value, secrets: &[&str]) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| {
                    let key = key.to_ascii_lowercase().replace(['_', '-'], "");
                    ![
                        "accesstoken",
                        "refreshtoken",
                        "sgcookie",
                        "cookie",
                        "authorization",
                        "password",
                        "codeverifier",
                    ]
                    .iter()
                    .any(|name| key == *name)
                })
                .map(|(key, value)| (key.clone(), sanitize(value, secrets)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| sanitize(value, secrets))
                .collect(),
        ),
        Value::String(text) => json!(redact(text, secrets)),
        _ => value.clone(),
    }
}

pub fn event(error: &Error) -> Value {
    json!({ "error": { "message": error.message, "status": error.status.unwrap_or(502), "code": error.code,
        "upstream_code": error.upstream_code, "type": if error.status == Some(429) { "rate_limit_error" } else { "upstream_error" } } })
}
