//! 国内错误码按当前原生实现：认证/永久限制/审核/短时拥塞/额度耗尽分开。
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
        "quota exceeded",
        "quota exhausted",
        "usage limit",
        "请求过于频繁",
        "频率限制",
        "额度已用尽",
        "额度耗尽",
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
fn transient(code: Option<i64>) -> bool {
    matches!(code, Some(14003 | 6001 | 6002 | 6005 | 6006))
}
fn exhausted(code: Option<i64>) -> bool {
    matches!(code, Some(14001 | 14012 | 14013 | 14014 | 14018 | 14019))
}
fn next_day(now: i64) -> i64 {
    now.saturating_add(28_800_000)
        .div_euclid(86_400_000)
        .saturating_add(1)
        .saturating_mul(86_400_000)
        .saturating_sub(28_800_000)
}
fn reset_at(body: &Value, now: i64) -> Option<i64> {
    let ceiling = now.checked_add(8 * 86_400_000)?;
    business(body, 6)
        .into_iter()
        .chain(std::iter::once(body))
        .flat_map(|node| [node.get("resetAt"), node.get("reset_at")])
        .flatten()
        .filter_map(Value::as_i64)
        .find(|at| *at > now && *at <= ceiling)
        .or_else(|| {
            let at = super::quota_reset::parse_quota_reset_at(message(body));
            (at > now && at <= ceiling).then_some(at)
        })
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
    if matches!(status, 499 | 504)
        || body["providerCode"].as_str().is_some_and(|code| {
            matches!(
                code,
                "aborted" | "cancelled" | "canceled" | "timeout" | "deadline_exceeded"
            ) || code.contains("cleanup")
        })
    {
        return json!({ "kind": "fatal", "status": status.max(400), "message": message, "upstream_code": code });
    }
    if status == 401 || code == Some(12153) {
        return json!({ "kind": "token_expired", "message": message });
    }
    if restricted(body)
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
        // 已有确定业务码沿用各自窗口；官方FAQ明确6003是限频、14003是模型侧繁忙。
        // https://www.workbuddy.ai/docs/zh/workbuddy/From-Beginner-to-Expert-Guide/FQA
        // 未知429没有明确限额证据时，即便带时刻也不能直接变成账号额度处罚。
        let evidence = status == 402
            || exhausted(code)
            || rate_limited(body)
            || matches!(code, Some(6000..=6008 | 14003 | 10105));
        let reset = (now > 0 && evidence)
            .then(|| {
                reset_at(body, now)
                    .or_else(|| (status == 402 || exhausted(code)).then(|| next_day(now)))
                    .or_else(|| transient(code).then(|| now.saturating_add(30_000)))
                    .or_else(|| (code == Some(10105)).then(|| now.saturating_add(60_000)))
            })
            .flatten();
        if let Some(reset) = reset {
            return json!({ "kind": "quota_limited", "status": if status == 402 { 402 } else { 429 },
                "message": message, "upstream_code": code, "reset_at": reset });
        }
        return json!({ "kind": "fatal", "status": if (400..600).contains(&status) { status } else { 429 },
            "message": message, "upstream_code": code });
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
        return json!({ "stop": false, "suspendAccount": true, "reason": "WorkBuddy已限制该账号，请向官方申诉；重新登录通常不能解除限制" });
    }
    let account_state =
        matches!(code, Some(14015 | 14016 | 14017 | 10105)) || status == 402 || exhausted(code);
    if account_state {
        let now = sdk::now();
        return json!({ "accountCooldownUntil": reset_at(body, now).unwrap_or_else(|| {
            if status == 402 || exhausted(code) { next_day(now) } else { now.saturating_add(60_000) }
        }), "reason": clean(message(body), &[]) });
    }
    Value::Null
}
pub fn retry(_status: u16, _body: &Value, _attempt: usize, _budget: usize) -> Value {
    json!({ "action": "default" })
}
