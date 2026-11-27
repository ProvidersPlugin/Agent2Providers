//! 上游嵌套 JSON/字符串错误先读语义再读 HTTP。只向主机输出四档分类。
//! 10605 排队是 503/fatal，不误当鉴权/额度；105 才是过期，裸 403 不自动续期。
use super::util;
use agent2api_wasm_sdk::{json, Error, Value};

const QUOTA: &[i64] = &[110, 112, 113, 114, 115, 116, 117, 118, 119, 122];
#[derive(Default)]
struct Signals {
    codes: Vec<i64>,
    queued: bool,
    retry: Option<u64>,
    text: String,
    content: bool,
    reset_at: Option<i64>,
}
fn scan(value: &Value, depth: usize, output: &mut Signals) {
    if depth > 6 {
        return;
    }
    match value {
        Value::Object(fields) => {
            for key in ["code", "statusCodeValue", "upstream_code", "upstreamCode"] {
                if let Some(code) = util::integer(&value[key]) {
                    output.codes.push(code);
                }
            }
            output.queued |= value["isQueued"] == true
                || value["serviceAvailable"] == false && value["queueType"].is_string();
            if output.retry.is_none() {
                output.retry = ["retryAfterSeconds", "waitTime"].iter().find_map(|key| {
                    util::integer(&value[*key]).and_then(|value| u64::try_from(value).ok())
                });
            }
            if output.reset_at.is_none() {
                output.reset_at = ["reset_at", "resetAt", "retryAt"]
                    .iter()
                    .find_map(|key| util::timestamp(&value[*key]));
            }
            output.content |=
                value["code"] == "content_filter" || value["type"] == "content_filter";
            for key in ["message", "body", "data", "error", "msg"] {
                if let Some(child) = fields.get(key) {
                    scan(child, depth + 1, output);
                }
            }
            if fields.contains_key("pricingUrl") {
                output.text.push_str(" pricingurl ");
            }
        }
        Value::String(text) => {
            if output.text.len() < 16 * 1024 {
                output.text.extend(text.chars().take(4096));
                output.text.push(' ');
            }
            if text.trim_start().starts_with('{') {
                if let Ok(value) = serde_json::from_str::<Value>(text) {
                    scan(&value, depth + 1, output);
                }
            }
        }
        _ => {}
    }
}
pub fn has_error(value: &Value) -> bool {
    envelope_error(value, 0)
}
fn envelope_error(value: &Value, depth: usize) -> bool {
    if depth > 6 {
        return false;
    }
    if let Some(text) = value
        .as_str()
        .filter(|text| text.len() <= 1024 * 1024 && text.trim_start().starts_with('{'))
    {
        return serde_json::from_str::<Value>(text)
            .ok()
            .is_some_and(|value| envelope_error(&value, depth + 1));
    }
    if !value.is_object() {
        return false;
    }
    if value.get("error").is_some_and(|error| {
        !error.is_null()
            && error != &Value::Bool(false)
            && error.as_str().is_none_or(|value| !value.trim().is_empty())
            && error.as_object().is_none_or(|value| !value.is_empty())
    }) || value["success"] == false
        || value["type"] == "error"
        || value["isQueued"] == true
        || value["serviceAvailable"] == false && value["queueType"].is_string()
    {
        return true;
    }
    if ["statusCodeValue", "code"].iter().any(|key| {
        let Some(raw) = value
            .get(*key)
            .filter(|value| !value.is_null() && *value != "")
        else {
            return false;
        };
        if raw == &Value::Bool(false)
            || raw.as_str().is_some_and(|text| text.trim().is_empty())
            || raw.as_object().is_some_and(|fields| fields.is_empty())
        {
            return false;
        }
        if *key == "code" && raw.is_boolean() {
            return false;
        }
        util::integer(raw)
            .map(|code| code != 0 && code != 200)
            .unwrap_or_else(|| !matches!(raw.as_str(), Some("SUCCESS" | "OK" | "success" | "ok")))
    }) {
        return true;
    }
    // 只钻信封，不扫描 choices/messages，避免把助手正常文本里的 code 当成错误。
    ["body", "data", "message", "msg"].iter().any(|key| {
        value
            .get(*key)
            .is_some_and(|child| envelope_error(child, depth + 1))
    })
}
pub fn classify(status: u16, body: &Value) -> Value {
    let mut signals = Signals::default();
    scan(body, 0, &mut signals);
    let text = signals.text.to_ascii_lowercase();
    let upstream = signals
        .codes
        .iter()
        .copied()
        .find(|code| *code == 10605 || *code == 105 || QUOTA.contains(code))
        .or_else(|| {
            signals
                .codes
                .iter()
                .copied()
                .find(|code| *code != 200 && *code != 0)
        });
    let (kind, http, code, message) = if signals.queued
        || signals.codes.contains(&10605)
        || body["code"] == "qwenwork_model_busy"
    {
        (
            "fatal",
            503,
            "qwenwork_model_busy",
            "千问办公 模型正在排队，这不是登录态失效或账号额度不足，请稍后重试",
        )
    } else if signals.codes.contains(&105) || body["code"] == "qwenwork_auth_expired" {
        (
            "token_expired",
            401,
            "qwenwork_auth_expired",
            "千问办公 登录态已失效，请续期或重新授权",
        )
    } else if signals.content {
        (
            "content_blocked",
            400,
            "content_filter",
            "千问办公 内容策略拒绝了当前请求，不是账号额度问题",
        )
    } else if status == 402
        || body["code"] == "qwenwork_balance_insufficient"
        || body["providerCode"] == "qwenwork_balance_insufficient"
        || [
            "积分余额不足",
            "余额不足",
            "积分不足",
            "credits exhausted",
            "credit exhausted",
            "credit balance",
            "insufficient balance",
            "insufficient credit",
        ]
        .iter()
        .any(|word| text.contains(*word))
    {
        (
            "quota_limited",
            402,
            "qwenwork_balance_insufficient",
            "千问办公 积分余额不足",
        )
    } else if signals.codes.iter().any(|code| QUOTA.contains(code))
        || body["code"] == "qwenwork_quota_limited"
        || [
            "pricingurl",
            "insufficient",
            "no_quota",
            "quota_exceed",
            "exceed_quota",
            "credit exhausted",
            "insufficient credit",
            "upgrade required",
            "subscription required",
            "plan does not support",
            "trial expired",
        ]
        .iter()
        .any(|word| text.contains(*word))
    {
        (
            "quota_limited",
            429,
            "qwenwork_quota_limited",
            "千问办公 当前账号额度不足或套餐不支持该模型",
        )
    } else if status == 429
        || body["code"] == "qwenwork_rate_limited"
        || text.contains("rate limit")
        || text.contains("too many")
    {
        (
            "quota_limited",
            429,
            "qwenwork_rate_limited",
            "千问办公 请求过于频繁，请稍后重试",
        )
    } else if status == 401 {
        (
            "token_expired",
            401,
            "qwenwork_auth_expired",
            "千问办公 登录态已失效，请续期或重新授权",
        )
    } else if signals.content
        || status >= 400
            && [
                "blocked by security policy",
                "unapproved channel",
                "illegal api invocation",
            ]
            .iter()
            .any(|word| text.contains(*word))
    {
        (
            "content_blocked",
            if status >= 400 { status } else { 400 },
            "content_filter",
            "千问办公 内容策略拒绝了当前请求，不是账号额度问题",
        )
    } else if status == 403 {
        (
            "fatal",
            403,
            "qwenwork_forbidden",
            "千问办公 拒绝访问，请确认当前账号的地区与模型权限",
        )
    } else {
        (
            "fatal",
            if (400..600).contains(&status) {
                status
            } else {
                502
            },
            "qwenwork_upstream_error",
            "千问办公 上游请求未完成或返回了无效结果",
        )
    };
    let reset_at = signals.reset_at.or_else(|| {
        signals
            .retry
            .filter(|_| code == "qwenwork_model_busy" || code == "qwenwork_rate_limited")
            .and_then(|seconds| seconds.checked_mul(1000))
            .and_then(|millis| i64::try_from(millis).ok())
            .and_then(|millis| util::now().ok()?.checked_add(millis))
    });
    json!({ "kind": kind, "status": http, "code": code, "message": message, "upstream_code": upstream,
        "reset_at": reset_at, "retry_after_ms": signals.retry.map(|seconds| seconds.clamp(5, 30) * 1000) })
}
pub fn queue_wait(body: &Value) -> u64 {
    let mut signals = Signals::default();
    scan(body, 0, &mut signals);
    signals.retry.unwrap_or(15).clamp(5, 30) * 1000
}
pub fn failure(status: u16, body: &Value) -> Error {
    let class = classify(status, body);
    let mut error = Error::http(
        class["status"].as_u64().unwrap_or(502) as u16,
        class["message"].as_str().unwrap_or("千问办公 请求失败"),
    )
    .with_code(class["code"].as_str().unwrap_or("qwenwork_upstream_error"));
    error.upstream_code = class["upstream_code"].as_i64();
    error.reset_at = class["reset_at"].as_i64();
    error
}
pub fn event(error: &Error) -> Value {
    json!({ "error": { "message": error.message, "status": error.status.unwrap_or(502), "code": error.code,
        "upstreamCode": error.upstream_code, "type": if error.status == Some(429) { "rate_limit_error" }
            else if error.status == Some(401) { "authentication_error" } else { "upstream_error" } } })
}
