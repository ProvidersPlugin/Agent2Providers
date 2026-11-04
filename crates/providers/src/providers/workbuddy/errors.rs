//! WorkBuddy 业务码决定错误语义；短时限流与授权限制不共用冷却。

use agent2api_provider_protocol as proto;
use serde_json::{json, Value};

use super::quota_reset::parse_quota_reset_at;

pub(super) fn code(body: &Value) -> Option<i64> {
    business_node(body, 6)?.get("code").and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
    })
}

fn business_node(body: &Value, depth: usize) -> Option<&Value> {
    if depth == 0 {
        return None;
    }
    if body
        .get("code")
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        })
        .is_some_and(|code| code >= 1000)
    {
        return Some(body);
    }
    [
        body.get("data"),
        body.get("error"),
        body.pointer("/response/data"),
    ]
    .into_iter()
    .flatten()
    .find_map(|node| business_node(node, depth - 1))
}

fn node_message(node: &Value) -> Option<&str> {
    ["message", "msg", "displayMsg"]
        .into_iter()
        .find_map(|key| {
            node.get(key)
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
        })
        .or_else(|| node.pointer("/error/message").and_then(Value::as_str))
}

fn message(body: &Value) -> &str {
    business_node(body, 6)
        .and_then(node_message)
        .or_else(|| node_message(body))
        .or_else(|| body.as_str())
        .unwrap_or("上游错误")
}

fn rate_limited(body: &Value) -> bool {
    let text = message(body).to_ascii_lowercase();
    [
        "rate limit",
        "rate-limiting",
        "rate-limited",
        "too many requests",
        "请求过于频繁",
        "限流",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn restricted(body: &Value) -> bool {
    code(body) == Some(11140) && {
        let text = message(body).to_ascii_lowercase();
        text.contains("request illegal")
            || text.contains("account has been restricted")
            || text.contains("unusual activity")
            || text.contains("账号存在异常行为")
    }
}

fn transient(code: Option<i64>) -> bool {
    matches!(code, Some(14003 | 6001 | 6002 | 6005 | 6006))
}

fn exhausted(code: Option<i64>) -> bool {
    matches!(code, Some(14001 | 14012 | 14013 | 14014 | 14018 | 14019))
}

fn next_day_reset() -> i64 {
    let now = crate::logging::now_ms();
    let day = 86_400_000;
    (now + 8 * 3_600_000)
        .div_euclid(day)
        .saturating_add(1)
        .saturating_mul(day)
        .saturating_sub(8 * 3_600_000)
}

pub(super) fn classify(status: u16, body: &Value) -> proto::ErrorClass {
    let code = code(body);
    let raw = message(body);
    let message = format!("上游返回 {status}: {raw}");
    if status == 401 || code == Some(12153) {
        return proto::ErrorClass::TokenExpired { message };
    }
    if restricted(body)
        || matches!(
            code,
            Some(14015 | 14016 | 14017 | 11115 | 11101 | 11133 | 11158)
        )
    {
        return proto::ErrorClass::Fatal {
            status: status.max(400),
            message,
            upstream_code: code,
        };
    }
    if code == Some(11128) || crate::content_block::matched(status, body) {
        return proto::ErrorClass::ContentBlocked {
            status: status.max(400),
            message,
            upstream_code: code,
        };
    }
    if status == 402
        || exhausted(code)
        || status == 429
        || rate_limited(body)
        || matches!(code, Some(6000..=6008 | 14003 | 10105))
    {
        let now = crate::logging::now_ms();
        let parsed = parse_quota_reset_at(raw);
        // 上游没在文案里给出恢复时刻时，**不要在这里猜一个固定值**：交给网关
        // （`mark_rate_limited`）按连击次数做指数退避 —— 首撞仍是 10 分钟，
        // 连续撞则 20 / 30 分钟，避免「10 分钟后回来又撞」的循环（#451）。
        // 例外是下面三种**确定的**语义：额度尽按次日重置、transient 短暂繁忙
        // 按 30 秒、10105 按 60 秒 —— 它们不是「不知道多久」，而是「知道大概
        // 多久」，所以照旧给具体时刻。
        let reset = if parsed > now {
            Some(parsed)
        } else if status == 402 || exhausted(code) {
            Some(next_day_reset())
        } else if transient(code) {
            Some(now.saturating_add(30_000))
        } else if code == Some(10105) {
            Some(now.saturating_add(60_000))
        } else {
            None
        };
        return proto::ErrorClass::QuotaLimited {
            status: if status == 402 { 402 } else { 429 },
            reset_at: reset,
            message,
            upstream_code: code,
        };
    }
    proto::ErrorClass::Fatal {
        status: status.max(400),
        message,
        upstream_code: code,
    }
}

pub(super) fn policy(status: u16, body: &Value) -> Value {
    let code = code(body);
    if restricted(body) {
        return json!({
            "stop": true,
            "suspendAccount": true,
            "reason": "WorkBuddy 已限制此账号的对话权限，请向官方申诉；重新登录通常不能解除限制",
        });
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
    if matches!(code, Some(14015 | 14016 | 14017 | 10105)) || status == 402 || exhausted(code) {
        return json!({
            "accountCooldownUntil": if status == 402 || exhausted(code) { next_day_reset() }
                else { crate::logging::now_ms().saturating_add(60_000) },
            "reason": message(body),
        });
    }
    Value::Null
}

pub(super) fn retry_denied(body: &Value) -> bool {
    [
        "",
        "/data",
        "/error",
        "/error/data",
        "/error/error",
        "/error/error/data",
    ]
    .iter()
    .filter_map(|path| {
        if path.is_empty() {
            Some(body)
        } else {
            body.pointer(path)
        }
    })
    .find_map(|node| node.get("retry_policy"))
    .and_then(|policy| policy.get("retry"))
    .and_then(Value::as_bool)
        == Some(false)
}

pub(super) fn retry(
    status: u16,
    body: &Value,
    attempt: usize,
    budget: usize,
) -> agent2api_provider_sdk::RetryDecision {
    if retry_denied(body) || policy(status, body).get("stop").and_then(Value::as_bool) == Some(true)
    {
        return agent2api_provider_sdk::RetryDecision::Stop {
            reason: "WorkBuddy 请求无法通过原样重发修复".to_string(),
        };
    }
    if transient(code(body)) && attempt < budget {
        return agent2api_provider_sdk::RetryDecision::Retry {
            delay_ms: 500_u64.saturating_mul(1_u64 << attempt.min(6)).min(32_000),
            reason: "WorkBuddy 模型繁忙或短时速率限制，按预算退避后重试".to_string(),
        };
    }
    agent2api_provider_sdk::RetryDecision::Default
}
