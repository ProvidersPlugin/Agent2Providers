//! 供 WASM 提供商在 `provider.info` 中声明统一错误分类映射与默认处理规则。

use serde_json::{json, Value};

pub fn class(kind: &str, statuses: &[u16], codes: &[&str]) -> Value {
    json!({ "kind": kind, "statuses": statuses, "codes": codes })
}

pub fn class_scoped(kind: &str, statuses: &[u16], codes: &[&str], scope: &str) -> Value {
    json!({ "kind": kind, "statuses": statuses, "codes": codes, "scope": scope })
}

#[allow(clippy::too_many_arguments)]
pub fn rule(
    id: &str,
    name: &str,
    reason: &str,
    kind: &str,
    statuses: &[u16],
    code: &str,
    action: &str,
    cooldown: &str,
    fallback_seconds: u64,
    exponential: bool,
    fallback_cap_seconds: u64,
    scope: &str,
) -> Value {
    json!({
        "id": id,
        "name": name,
        "reason": reason,
        "kind": kind,
        "statuses": statuses,
        "code": code,
        "model": "",
        "enabled": true,
        "action": action,
        "cooldown": cooldown,
        "fallbackSeconds": fallback_seconds,
        "exponential": exponential,
        "fallbackCapSeconds": fallback_cap_seconds,
        "scope": scope,
    })
}

pub fn attach(mut meta: Value, extra_classes: Vec<Value>, extra_rules: Vec<Value>) -> Value {
    let refresh = meta["capabilities"]["refresh"].as_bool() == Some(true);
    let defaults = vec![
        rule(
            "rate_limit",
            "请求限流（429）",
            "上游触发限流时优先按上游恢复时间冷却；未提供时间时短冷却并逐次延长等待，立即切换可用账号。",
            "rate_limit",
            &[429],
            "",
            "switch_account",
            "upstream",
            60,
            true,
            900,
            "auto",
        ),
        rule(
            "quota",
            "额度耗尽",
            "账号周期额度耗尽时优先按上游重置时间冷却并切换账号；缺失时间时按兜底冷却处理并延长间隔。",
            "quota",
            &[],
            "",
            "switch_account",
            "upstream",
            600,
            true,
            3600,
            "auto",
        ),
        rule(
            "balance",
            "余额或积分不足",
            "当前账号余额或积分不足时按账号冷却并递增等待，余额回升后自动恢复，同时切换到其他可用账号。",
            "balance",
            &[],
            "",
            "switch_account",
            "upstream",
            600,
            true,
            21_600,
            "account",
        ),
        if refresh {
            rule(
                "auth",
                "登录态失效",
                "检测到凭据失效时先尝试自动续期一次，仍失败则冷却该账号并切换其他可用账号。",
                "auth",
                &[401],
                "",
                "refresh_then_switch",
                "fixed",
                300,
                true,
                3600,
                "account",
            )
        } else {
            rule(
                "auth",
                "登录态失效",
                "当前提供商不支持自动续期；登录失效后冷却该账号并立即切换其他可用账号。",
                "auth",
                &[401],
                "",
                "switch_account",
                "fixed",
                600,
                true,
                3600,
                "account",
            )
        },
        rule(
            "transient",
            "连接或服务异常",
            "上游网络抖动或临时服务异常时先按重发策略重试，失败后进入保护退避并切换账号。",
            "transient",
            &[408, 425, 500, 502, 503, 504, 520, 521, 522, 524, 529],
            "",
            "retry_then_switch",
            "protection",
            60,
            true,
            600,
            "model",
        ),
        rule(
            "unsupported",
            "权限或能力受限（403 / 501）",
            "当前账号或模型权限不足、功能不被上游支持时，将该对象优先级调到最低并跳过切换。",
            "unsupported",
            &[403, 501],
            "",
            "switch_provider",
            "deprioritize",
            1800,
            true,
            7200,
            "model",
        ),
        rule(
            "bad_request",
            "请求参数不兼容（400 / 413 / 422）",
            "上游拒绝当前参数或请求规模时，将该提供商优先级调到最低并切换其他兼容提供商。",
            "bad_request",
            &[400, 413, 422],
            "",
            "switch_provider",
            "deprioritize",
            1800,
            true,
            7200,
            "provider",
        ),
        rule(
            "content",
            "内容策略拒绝",
            "命中上游内容策略限制时，将该提供商优先级调到最低并切换其他提供商。",
            "content",
            &[],
            "",
            "switch_provider",
            "deprioritize",
            1800,
            false,
            1800,
            "provider",
        ),
    ];

    let mut rules = Vec::new();
    for item in &extra_rules {
        let id = item["id"].as_str().unwrap_or("");
        if !defaults.iter().any(|base| base["id"].as_str() == Some(id)) {
            rules.push(item.clone());
        }
    }
    for base in defaults {
        let id = base["id"].as_str().unwrap_or("");
        if let Some(replacement) = extra_rules.iter().find(|item| item["id"].as_str() == Some(id)) {
            rules.push(replacement.clone());
        } else {
            rules.push(base);
        }
    }

    if let Some(obj) = meta.as_object_mut() {
        if !extra_classes.is_empty() {
            obj.insert("error_classifications".to_string(), Value::Array(extra_classes));
        }
        obj.insert("error_rules".to_string(), Value::Array(rules));
    }
    meta
}
