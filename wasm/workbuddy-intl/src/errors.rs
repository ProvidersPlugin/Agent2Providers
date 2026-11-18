//! 原生WorkBuddy两区共用的错误码事实，国际包独立保存分类与冷却状态；不借用国内额度。
use super::util;
use agent2api_wasm_sdk::{self as sdk, json, Error, Value};

pub fn code(value: &Value) -> Option<i64> {
    business(value, 6)
        .and_then(|v| util::integer(&v["code"]))
        .or_else(|| util::integer(&value["code"]))
}
fn business(value: &Value, depth: usize) -> Option<&Value> {
    if depth == 0 {
        return None;
    }
    if util::integer(&value["code"]).is_some_and(|code| code >= 1000) {
        return Some(value);
    }
    [
        value.get("data"),
        value.get("error"),
        value.pointer("/response/data"),
    ]
    .into_iter()
    .flatten()
    .find_map(|v| business(v, depth - 1))
}
fn node_message(value: &Value) -> Option<&str> {
    ["message", "msg", "displayMsg"]
        .iter()
        .find_map(|key| value[*key].as_str().filter(|s| !s.trim().is_empty()))
        .or_else(|| value.pointer("/error/message").and_then(Value::as_str))
}
pub fn message(value: &Value) -> &str {
    business(value, 6)
        .and_then(node_message)
        .or_else(|| node_message(value))
        .or_else(|| value.as_str())
        .unwrap_or("上游未返回错误说明")
}
pub fn failed(status: u16, value: &Value) -> bool {
    if let Some(raw) = value.get("code").filter(|raw| !raw.is_null()) {
        // 业务码只能是整数，0.5不能向下取整成成功0。
        if util::number(raw).is_none_or(|code| code.fract() != 0.0) {
            return true;
        }
    }
    !(200..300).contains(&status)
        || code(value).is_some_and(|code| code != 0)
        || value.get("error").is_some_and(|error| !error.is_null())
}
fn rate_limited(body: &Value) -> bool {
    let message = message(body).to_ascii_lowercase();
    [
        "rate limit",
        "rate-limiting",
        "rate-limited",
        "too many requests",
        "请求过于频繁",
        "限流",
    ]
    .iter()
    .any(|part| message.contains(part))
}
fn restricted(body: &Value) -> bool {
    let text = message(body).to_ascii_lowercase();
    code(body) == Some(11140)
        && [
            "request illegal",
            "account has been restricted",
            "unusual activity",
            "账号存在异常行为",
        ]
        .iter()
        .any(|s| text.contains(s))
}
pub fn channel_rejected(body: &Value) -> bool {
    let text = message(body).to_ascii_lowercase();
    [
        "first message is not system",
        "conversation id missing",
        "channel verification",
        "not sent from an official client",
        "unapproved channel",
        "illegal api invocation",
        "渠道校验",
        "不是官方客户端",
        "非官方客户端",
    ]
    .iter()
    .any(|part| text.contains(part))
        || code(body) == Some(11128) && text.contains("request illegal")
}
fn transient(code: Option<i64>) -> bool {
    matches!(code, Some(14003 | 6001 | 6002 | 6005 | 6006))
}
fn exhausted(code: Option<i64>) -> bool {
    matches!(code, Some(14001 | 14012 | 14013 | 14014 | 14018 | 14019))
}
fn next_day() -> i64 {
    (sdk::now() + 28_800_000)
        .div_euclid(86_400_000)
        .saturating_add(1)
        .saturating_mul(86_400_000)
        .saturating_sub(28_800_000)
}
pub fn clean(message: &str, secrets: &[&str]) -> String {
    let mut message = message.to_string();
    let mut secrets: Vec<_> = secrets.iter().copied().filter(|s| !s.is_empty()).collect();
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for secret in secrets {
        message = message.replace(secret, "[已隐藏]");
    }
    message.chars().take(2000).collect()
}
fn classified(status: u16, body: &Value, secrets: &[&str]) -> Value {
    let code = code(body);
    let raw = message(body);
    let message = clean(&format!("上游返回 {status}：{raw}"), secrets);
    if status == 401 || code == Some(12153) {
        return json!({ "kind": "token_expired", "message": message });
    }
    if restricted(body)
        || channel_rejected(body)
        || matches!(
            code,
            Some(14015 | 14016 | 14017 | 11115 | 11101 | 11133 | 11158)
        )
    {
        return json!({ "kind": "fatal", "status": status.max(400), "message": message, "upstream_code": code });
    }
    let blocked = code == Some(11128)
        || body["providerCode"] == "workbuddy_content_blocked"
        || [
            "blocked by security policy",
            "unapproved channel",
            "illegal api invocation",
        ]
        .iter()
        .any(|s| raw.to_ascii_lowercase().contains(s));
    if blocked {
        return json!({ "kind": "content_blocked", "status": status.max(400), "message": message, "upstream_code": code });
    }
    if status == 402
        || exhausted(code)
        || status == 429
        || rate_limited(body)
        || matches!(code, Some(6000..=6008 | 14003 | 10105))
    {
        let now = sdk::now();
        let parsed = body["resetAt"]
            .as_i64()
            .or_else(|| body["reset_at"].as_i64())
            .filter(|at| *at > now)
            .unwrap_or_else(|| super::quota_reset::parse_quota_reset_at(raw));
        let reset = if parsed > now {
            Some(parsed)
        } else if status == 402 || exhausted(code) {
            Some(next_day())
        } else if transient(code) {
            Some(now.saturating_add(30_000))
        } else if code == Some(10105) {
            Some(now.saturating_add(60_000))
        } else {
            None
        };
        return json!({ "kind": "quota_limited", "status": if status == 402 { 402 } else { 429 }, "message": message, "upstream_code": code, "reset_at": reset });
    }
    json!({ "kind": "fatal", "status": if (400..600).contains(&status) { status } else { 502 }, "message": message, "upstream_code": code })
}
pub fn classify(status: u16, body: &Value) -> Value {
    classified(status, body, &[])
}
pub fn upstream(status: u16, body: &Value, secrets: &[&str]) -> Error {
    let class = classified(status, body, secrets);
    let status = if class["kind"] == "token_expired" {
        401
    } else {
        class["status"].as_u64().unwrap_or(502) as u16
    };
    let mut error = Error::http(
        status,
        class["message"].as_str().unwrap_or("WorkBuddy 请求失败"),
    )
    .with_code(if class["kind"] == "content_blocked" {
        "workbuddy_content_blocked"
    } else {
        "workbuddy_upstream"
    });
    error.upstream_code = code(body);
    error.reset_at = class["reset_at"].as_i64();
    error
}
pub fn policy(status: u16, body: &Value) -> Value {
    let code = code(body);
    if restricted(body) {
        return json!({ "stop": true, "suspendAccount": true, "reason": "WorkBuddy已限制该账号，请向官方申诉；重新登录通常不能解除限制" });
    }
    if channel_rejected(body) {
        return json!({ "stop": true, "reason": "WorkBuddy国际渠道或消息格式不符合要求，未重发相同请求" });
    }
    let account_state =
        matches!(code, Some(14015 | 14016 | 14017 | 10105)) || status == 402 || exhausted(code);
    if !account_state
        && !transient(code)
        && !rate_limited(body)
        && !matches!(code, Some(6000..=6008))
        && (matches!(code, Some(11115 | 11101 | 11133 | 11158))
            || matches!(status, 400 | 413 | 422) && code != Some(11128))
    {
        return json!({ "stop": true, "reason": "请求参数、上下文或模型权限不满足上游要求，未重发相同请求" });
    }
    if account_state {
        let reset = classify(status, body)["reset_at"]
            .as_i64()
            .filter(|at| *at > sdk::now());
        return json!({ "accountCooldownUntil": reset.unwrap_or_else(|| {
            if status == 402 || exhausted(code) { next_day() } else { sdk::now().saturating_add(60_000) }
        }), "reason": clean(message(body), &[]) });
    }
    Value::Null
}
pub fn retry(status: u16, body: &Value, attempt: usize, budget: usize) -> Value {
    let denied = [
        "",
        "/data",
        "/error",
        "/error/data",
        "/error/error",
        "/error/error/data",
    ]
    .iter()
    .filter_map(|p| {
        if p.is_empty() {
            Some(body)
        } else {
            body.pointer(p)
        }
    })
    .find_map(|v| v.get("retry_policy"))
    .and_then(|v| v["retry"].as_bool())
        == Some(false);
    if denied || policy(status, body)["stop"] == true {
        return json!({ "action": "stop", "reason": "WorkBuddy请求不能通过原样重发修复" });
    }
    if attempt < budget {
        if transient(code(body)) {
            return json!({ "action": "retry", "delay_ms": 500u64.saturating_mul(1 << attempt.min(6)).min(32_000), "reason": "模型短暂繁忙，按预算退避" });
        }
        if code(body) == Some(11128) {
            return json!({ "action": "retry", "delay_ms": 10_000, "reason": "上游敏感词拦截（11-128），请检查提示词" });
        }
    }
    json!({ "action": "default" })
}
