//! 1.1.5 的 accountRequests 与 Box-Agent 区分鉴权、积分不足和短期限流。

use agent2api_provider_protocol as proto;
use serde_json::Value;

fn code_value(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse().ok())
}

pub(super) fn business_code(body: &Value) -> Option<i64> {
    body.get("code")
        .and_then(code_value)
        .or_else(|| body.pointer("/error/code").and_then(code_value))
}

fn code_text(body: &Value) -> String {
    body.get("code")
        .or_else(|| body.pointer("/error/code"))
        .map(super::jwt::js_text)
        .unwrap_or_default()
        .to_ascii_lowercase()
}

pub(super) fn message(body: &Value) -> String {
    ["/message", "/msg", "/error/message", "/error"]
        .iter()
        .find_map(|path| {
            body.pointer(path)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "上游未返回错误说明".to_string())
}

pub(super) fn token_expired(body: &Value) -> bool {
    matches!(business_code(body), Some(200_003 | 200_009))
        || code_text(body) == "authorization_verify_error"
        || message(body).contains("authorization_verify_error")
}

pub(super) fn classify(status: u16, body: &Value) -> proto::ErrorClass {
    let raw = message(body);
    let message = format!("上游返回 {status}: {raw}");
    if status == 401 || token_expired(body) {
        return proto::ErrorClass::TokenExpired { message };
    }
    let code = code_text(body);
    let lowered = raw.to_ascii_lowercase();
    let quota = business_code(body) == Some(1_000_007)
        || ["insufficient_points", "insufficient_quota", "arrearage"].contains(&code.as_str())
        || [
            "insufficient_points",
            "insufficient quota",
            "积分不足",
            "余额不足",
            "欠费",
        ]
        .iter()
        .any(|marker| lowered.contains(marker));
    if quota || status == 429 {
        let transient = !quota
            && (code == "rate_limit"
                || [
                    "rate limit",
                    "too many requests",
                    "请求过于频繁",
                    "频率",
                    "rpm",
                    "tpm",
                ]
                .iter()
                .any(|marker| lowered.contains(marker)));
        return proto::ErrorClass::QuotaLimited {
            reset_at: transient.then(|| crate::logging::now_ms() + 60_000),
            message,
            upstream_code: business_code(body),
            status,
        };
    }
    crate::content_block::classify_or_fatal(status, body, message, business_code(body))
}
