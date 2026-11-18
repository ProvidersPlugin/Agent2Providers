//! Command Code HTTP/NDJSON 的真实错误分类；套餐拒绝不冒充Key失效，流内错误不转空成功。
use agent2api_wasm_sdk::{json, Error, Value};

const CONTENT_PATTERNS: &[&str] = &[
    "blocked by security policy",
    "unapproved channel",
    "illegal api invocation",
    "content_policy_refused",
];

fn message(body: &Value) -> String {
    body["error"]["message"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            body["message"]
                .as_str()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| {
            body["error"]
                .as_str()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| body.as_str().filter(|value| !value.trim().is_empty()))
        .unwrap_or("Command Code 上游错误")
        .chars()
        .take(8192)
        .collect()
}

fn upstream_code(body: &Value) -> Option<i64> {
    [
        body.get("upstreamCode"),
        body.get("upstream_code"),
        body.get("code"),
        body["error"].get("code"),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_i64)
}

fn code(body: &Value) -> String {
    body["error"]["code"]
        .as_str()
        .or_else(|| body["code"].as_str())
        .or_else(|| body["error"]["type"].as_str())
        .or_else(|| body["type"].as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

pub fn authentication_failure(body: &Value) -> bool {
    let lower = message(body).to_ascii_lowercase();
    matches!(
        code(body).as_str(),
        "unauthorized" | "authentication_error" | "invalid_api_key" | "invalid_token"
    ) || [
        "invalid 'authorization' header",
        "invalid api key",
        "invalid token",
        "api key has expired",
        "not signed in",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
}

fn numeric(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
        .filter(|value| value.is_finite())
}

pub fn timestamp(value: &Value) -> Option<i64> {
    if let Some(raw) = value.as_str() {
        if let Ok(at) = chrono::DateTime::parse_from_rfc3339(raw) {
            return Some(at.timestamp_millis()).filter(|at| *at > 0);
        }
    }
    numeric(value)
        .filter(|at| *at > 0.0 && *at <= 8.64e15)
        .map(|at| {
            if at < 1e12 {
                (at * 1000.0) as i64
            } else {
                at as i64
            }
        })
}

fn reset_at(body: &Value) -> Option<i64> {
    for value in [
        body.get("reset_at"),
        body.get("resetAt"),
        body["error"].get("reset_at"),
        body["error"].get("resetAt"),
    ] {
        if let Some(at) = value.and_then(timestamp) {
            return Some(at);
        }
    }
    [
        body.get("retry_after"),
        body.get("retryAfter"),
        body["error"].get("retry_after"),
    ]
    .into_iter()
    .flatten()
    .find_map(numeric)
    .filter(|seconds| *seconds > 0.0 && *seconds <= 31_536_000.0)
    .map(|seconds| agent2api_wasm_sdk::now().saturating_add((seconds * 1000.0) as i64))
}

pub fn classify(status: u16, body: &Value) -> Value {
    let raw = message(body);
    let lower = raw.to_ascii_lowercase();
    let code = code(body);
    let blocked = matches!(
        code.as_str(),
        "content_filter" | "content_policy_refused" | "content_blocked"
    ) || CONTENT_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern));
    let quota = status == 402
        || status == 429
        || [
            "premium_credits_exhausted",
            "insufficient credits",
            "insufficient_credits",
            "rate_limited",
            "usage limit",
            "window_limit",
            "model_not_in_plan",
            "upgrade_required",
        ]
        .iter()
        .any(|pattern| lower.contains(pattern) || code.contains(pattern));
    let (kind, mapped) = if blocked {
        ("content_blocked", 400)
    } else if quota {
        ("quota_limited", 429)
    } else if status == 401 || authentication_failure(body) {
        ("token_expired", 401)
    } else {
        (
            "fatal",
            match status {
                400 | 403 | 404 | 413 | 422 | 503 => status,
                _ => 502,
            },
        )
    };
    json!({ "kind": kind, "status": mapped, "message": raw,
        "upstream_code": upstream_code(body), "reset_at": reset_at(body) })
}

pub fn http_error(status: u16, body: &Value) -> Error {
    let class = classify(status, body);
    let mapped = class["status"].as_u64().unwrap_or(502) as u16;
    let reported = code(body);
    let safe_reported = !reported.is_empty()
        && reported.len() <= 128
        && reported
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte));
    let fallback = match class["kind"].as_str() {
        Some("content_blocked") => "content_filter",
        Some("token_expired") => "authentication_error",
        Some("quota_limited") => "rate_limit_error",
        _ if mapped == 400 || mapped == 422 => "invalid_request_error",
        _ if mapped == 403 => "permission_denied",
        _ if mapped == 404 => "not_found",
        _ if mapped == 413 => "request_too_large",
        _ => "upstream_error",
    };
    let mut error = Error::http(
        mapped,
        class["message"].as_str().unwrap_or("Command Code 上游错误"),
    )
    .with_code(if safe_reported {
        reported.as_str()
    } else {
        fallback
    });
    error.upstream_code = upstream_code(body);
    error.reset_at = reset_at(body);
    error
}

fn status(value: &Value) -> Option<u16> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
        .and_then(|value| u16::try_from(value).ok())
        .filter(|status| (400..600).contains(status))
}

pub fn event_error(event: &Value) -> Error {
    let mut body = event.clone();
    let raw = message(event);
    let trimmed = raw.trim().trim_start_matches('<');
    let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    let embedded = (digits.len() == 3)
        .then(|| digits.parse::<u16>().ok())
        .flatten()
        .filter(|status| (400..600).contains(status));
    if let Some(start) = raw.find('{') {
        if let Ok(parsed) = serde_json::from_str::<Value>(&raw[start..]) {
            if parsed.is_object() {
                body = parsed;
            }
        }
    }
    let reported = embedded
        .or_else(|| status(&event["error"]["statusCode"]))
        .or_else(|| status(&event["statusCode"]))
        .or_else(|| status(&event["error"]["status"]))
        .or_else(|| status(&event["status"]))
        .unwrap_or(502);
    http_error(reported, &body)
}

pub fn error_event(error: &Error) -> Value {
    let status = error.status.unwrap_or(502);
    let mut frame = json!({ "error": { "message": error.message, "status": status,
        "type": error.code, "code": error.code, "upstreamCode": error.upstream_code, "resetAt": error.reset_at } });
    if let Some(reset) = error.reset_at.filter(|at| *at > agent2api_wasm_sdk::now()) {
        frame["retry_after"] =
            json!((reset - agent2api_wasm_sdk::now()).saturating_add(999) / 1000);
    }
    frame
}

pub fn host_failure(error: &Error) -> bool {
    matches!(
        error.code.as_deref(),
        Some(
            "forbidden"
                | "aborted"
                | "ABORT_ERR"
                | "cancelled"
                | "timeout"
                | "plugin_unavailable"
                | "plugin_resource_limit"
                | "plugin_protocol_error"
                | "account_changed"
        )
    ) || error.status == Some(499)
}

pub fn redact_text(text: &str, key: &str) -> String {
    let input = if key.is_empty() {
        text.to_string()
    } else {
        text.replace(key, "[redacted]")
    };
    let mut output = String::new();
    let mut rest = input.as_str();
    while let Some(start) = rest.find("user_") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 5..];
        let length = after
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(byte))
            .count();
        if length > 0 {
            output.push_str("[redacted]");
        } else {
            output.push_str("user_");
        }
        rest = &after[length..];
    }
    output.push_str(rest);
    output.chars().take(4096).collect()
}

pub fn redact(error: &mut Error, key: &str) {
    error.message = redact_text(&error.message, key);
}
