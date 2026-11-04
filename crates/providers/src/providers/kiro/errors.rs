//! Kiro 的错误分类：四档映射与「输入过长」的识别。
//!
//! ── 上游的表达方式 ──────────────────────────────────────────
//! ```text
//!   400  ValidationException "Input is too long."（reason CONTENT_LENGTH_EXCEEDS_THRESHOLD）
//!   429  月额度用完 / 请求数超限（USAGE_LIMIT / MONTHLY_REQUEST_COUNT）
//!   503  INSUFFICIENT_MODEL_CAPACITY（模型没容量）
//!   403  token 被拒（刷新后重试一次）
//!   exception 帧  throttlingException / validationException / …
//! ```
//!
//! ── 「输入过长」必须原样透传 ────────────────────────────────
//! 它会被当成限额（`exceeded`）而罚账号，但实际是客户端上下文太长：换账号
//! 没用，只会让另一个账号白撞一次。参考实现把它统一成 400 + 固定前缀，
//! 我们沿用这句话（网关与客户端都按它判「该压缩上下文」）。

use agent2api_provider_protocol as proto;

/// 「输入过长」的统一前缀（与参考实现同文）。
pub const TOO_LONG: &str = "input is too long for the model's context: ";

/// 文案里是否在说「对话超出模型窗口」。
pub fn too_long_said(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("content_length_exceeds_threshold")
        || lower.contains("model_context_window_exceeded")
        || lower.contains("input is too long")
}

/// 文案里是否在说「额度 / 限流」。
fn quota_said(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "quota",
        "insufficient",
        "balance",
        "credit",
        "billing",
        "usage_limit",
        "usage limit",
        "monthly_request_count",
        "rate limit",
        "rate limited",
        "too many requests",
        "throttl",
        "limit reached",
        "overloaded",
        "额度",
        "限流",
        "频率",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

/// HTTP 状态 + 正文 → 上游失败（状态, 文案）。
///
/// 与参考实现的 `failure()` 同口径：解析 `{"message","reason"}`，把
/// 「输入过长」抬成 400、把容量不足保持 503、把额度抬成 429。
pub fn failure(status: u16, body: &str) -> (u16, String) {
    let mut message = body.trim().to_string();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(text) = value.get("message").and_then(|value| value.as_str()) {
            message = text.to_string();
            if let Some(reason) = value.get("reason").and_then(|value| value.as_str()) {
                if !reason.is_empty() {
                    message = format!("{message} ({reason})");
                }
            }
        }
    }
    if message.is_empty() {
        message = status_text(status);
    }
    if too_long_said(&message) {
        return (400, format!("{TOO_LONG}{message}"));
    }
    if message.contains("INSUFFICIENT_MODEL_CAPACITY") {
        return (503, message);
    }
    if message.contains("MONTHLY_REQUEST_COUNT") || message.contains("USAGE_LIMIT") {
        return (429, format!("usage limit reached: {message}"));
    }
    (status, message)
}

/// 常见状态码的标准文案（Go 的 `http.StatusText` 同款）。
pub fn status_text(status: u16) -> String {
    match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Request Entity Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown Error",
    }
    .to_string()
}

/// 四档分类（供网关 `classify` 用）。
///
/// 注意：本家 `stateful = true`，网关的会话路径主要以插件推出的错误帧为准；
/// 这个分类仍用于 `classify` 契约与流内错误。
pub fn classify(status: u16, body: &serde_json::Value) -> proto::ErrorClass {
    let message = body
        .get("message")
        .and_then(|value| value.as_str())
        .or_else(|| body.get("error").and_then(|value| value.as_str()))
        .unwrap_or("")
        .to_string();
    if status == 401 || status == 403 {
        return proto::ErrorClass::TokenExpired {
            message: if message.is_empty() {
                "Kiro 的登录态已失效，请重新登录".to_string()
            } else {
                message
            },
        };
    }
    if status == 429 || quota_said(&message) {
        return proto::ErrorClass::QuotaLimited {
            reset_at: None,
            message: if message.is_empty() {
                "Kiro 额度或频率已到上限".to_string()
            } else {
                message
            },
            upstream_code: None,
            status,
        };
    }
    proto::ErrorClass::Fatal {
        status,
        message,
        upstream_code: None,
    }
}

/// 流内错误的文案整理（与参考实现的 `exception()` 同口径）。
pub fn exception_text(kind: &str, payload: &str) -> String {
    let mut message = payload.trim().to_string();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) {
        if let Some(text) = value
            .get("message")
            .and_then(|value| value.as_str())
            .or_else(|| value.get("Message").and_then(|value| value.as_str()))
        {
            message = text.to_string();
        }
        if let Some(reason) = value.get("reason").and_then(|value| value.as_str()) {
            if !reason.is_empty() {
                message = format!("{message} ({reason})");
            }
        }
    }
    if message.is_empty() {
        message = "Kiro 返回了一个空的错误".to_string();
    }
    if kind.to_ascii_lowercase().contains("throttl") {
        return format!("rate limited: {message}");
    }
    if kind.is_empty() {
        message
    } else {
        format!("{kind}: {message}")
    }
}

/// 流内错误是否按额度处理（换账号／冷却）。
pub fn stream_error_is_quota(text: &str) -> bool {
    quota_said(text) && !too_long_said(text)
}

/// 流内错误是否是输入过长（透传，不罚账号）。
pub fn stream_error_is_too_long(text: &str) -> bool {
    too_long_said(text)
}
