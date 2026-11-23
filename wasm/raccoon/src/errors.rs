//! 1.1.5业务错误分类：认证、积分不足、60秒窗口限流与内容策略；HTTP200信封也检查。

use super::util;
use agent2api_wasm_sdk::{self as sdk, json, Error, Value};

pub fn code(body: &Value) -> Option<i64> {
    body.get("code")
        .and_then(util::integer)
        .or_else(|| body.pointer("/error/code").and_then(util::integer))
}
fn code_text(body: &Value) -> String {
    body.get("code")
        .or_else(|| body.pointer("/error/code"))
        .map(util::js)
        .unwrap_or_default()
        .to_ascii_lowercase()
}
pub fn message(body: &Value) -> String {
    ["/message", "/msg", "/error/message", "/error"]
        .iter()
        .find_map(|path| {
            body.pointer(path)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
        })
        .unwrap_or("上游未返回错误说明")
        .to_string()
}
pub fn authentication(body: &Value) -> bool {
    matches!(code(body), Some(200_003 | 200_009))
        || code_text(body) == "authorization_verify_error"
        || message(body).contains("authorization_verify_error")
}
pub fn failed(status: u16, body: &Value) -> bool {
    let code = code_text(body);
    !(200..300).contains(&status)
        || !code.is_empty() && code != "0"
        || body.get("error").is_some_and(|value| !value.is_null())
        || authentication(body)
}
pub fn classify(status: u16, body: &Value) -> Value {
    classified(status, body, &[])
}
fn classified(status: u16, body: &Value, secrets: &[&str]) -> Value {
    let raw = message(body);
    let message = clean(&format!("上游返回 {status}：{raw}"), secrets);
    if status == 401 || authentication(body) {
        return json!({ "kind": "token_expired", "message": message });
    }
    let text = code_text(body);
    let lower = raw.to_ascii_lowercase();
    let quota = code(body) == Some(1_000_007)
        || ["insufficient_points", "insufficient_quota", "arrearage"].contains(&text.as_str())
        || [
            "insufficient_points",
            "insufficient quota",
            "积分不足",
            "余额不足",
            "欠费",
        ]
        .iter()
        .any(|part| lower.contains(part));
    if quota || status == 429 {
        let transient = !quota
            && ([
                "rate_limit",
                "rate_limit_exceeded",
                "user_model_rate_limited",
            ]
            .contains(&text.as_str())
                || [
                    "rate limit",
                    "too many requests",
                    "请求过于频繁",
                    "频率",
                    "rpm",
                    "tpm",
                ]
                .iter()
                .any(|part| lower.contains(part)));
        return json!({ "kind": "quota_limited", "status": 429, "message": message, "upstream_code": code(body),
            "reset_at": body.get("resetAt").or_else(|| body.get("reset_at")).and_then(util::integer)
                .filter(|at| *at > sdk::now()).or_else(|| transient.then(|| sdk::now().saturating_add(60_000))) });
    }
    let status = if (400..600).contains(&status) {
        status
    } else {
        502
    };
    let blocked = body["providerCode"] == "raccoon_content_blocked"
        || [
            "blocked by security policy",
            "unapproved channel",
            "illegal api invocation",
        ]
        .iter()
        .any(|part| lower.contains(part));
    json!({ "kind": if blocked { "content_blocked" } else { "fatal" }, "status": status,
        "message": if blocked { format!("{message}；内容策略拒绝，不是账号额度问题，请调整内容后重试") } else { message }, "upstream_code": code(body) })
}
pub fn clean(text: &str, secrets: &[&str]) -> String {
    let mut text = text.to_string();
    let mut secrets: Vec<&str> = secrets
        .iter()
        .copied()
        .filter(|value| !value.is_empty())
        .collect();
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    for secret in secrets {
        text = text.replace(secret, "[已隐藏]");
    }
    // 先完整脱敏再截断，否则一枚跨越截断点的长令牌会留下可识别前缀。
    text.chars().take(2000).collect()
}
pub fn upstream(status: u16, payload: &Value, secrets: &[&str]) -> Error {
    let class = classified(status, payload, secrets);
    let kind = class["kind"].as_str().unwrap_or("fatal");
    let status = if kind == "token_expired" {
        401
    } else {
        class["status"].as_u64().unwrap_or(502) as u16
    };
    let mut error = Error::http(
        status,
        clean(
            class["message"].as_str().unwrap_or("小浣熊请求失败"),
            secrets,
        ),
    )
    .with_code(if kind == "content_blocked" {
        "raccoon_content_blocked"
    } else {
        "raccoon_upstream"
    });
    error.upstream_code = code(payload);
    error.reset_at = class["reset_at"].as_i64();
    error
}
pub fn image_uncertain(message: &str) -> Error {
    Error::http(
        502,
        format!("小浣熊图片结果尚未确认：{message}；未自动重新提交，以免重复消耗积分"),
    )
    .with_code("image_output_uncertain")
}
