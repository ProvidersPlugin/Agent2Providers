//! 原生 Get-Usage-Limits 双认证路径与月额度/免费试用/赠送积分读数。
//! 数字缺失就是未知；不能把缺少 currentUsage 的窗口算成「全额可用」。

use crate::{credentials::Credentials, endpoints, errors, network, refresh, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn fetch(c: &Credentials) -> Result<Value> {
    let base = endpoints::management(&c.runtime_region()?)?;
    if c.api_key() {
        return network::aws_json(
            c,
            &format!("{base}/?origin=KIRO_CLI"),
            endpoints::USAGE_TARGET,
            &json!({ "origin": "KIRO_CLI" }),
        );
    }
    let mut url =
        format!("{base}/Get-Usage-Limits?origin=KIRO_CLI&resourceType=CREDIT&isEmailRequired=true");
    if !c.profile.is_empty() {
        url.push_str(&format!("&profileArn={}", util::query(&c.profile)));
    }
    network::get(c, &url)
}
pub fn verified_email(payload: &Value, c: &Credentials) -> Result<String> {
    let email = payload["userInfo"]["email"]
        .as_str()
        .filter(|email| {
            !email.is_empty()
                && email.len() <= 400
                && !email
                    .chars()
                    .any(|ch| ch.is_control() || ch.is_whitespace())
                && email.split_once('@').is_some_and(|(local, domain)| {
                    !local.is_empty() && !domain.is_empty() && !domain.contains('@')
                })
                && errors::redact(email, &c.secrets()) == *email
        })
        .ok_or_else(|| Error::http(502, "Kiro 未返回可确认的账号邮箱，请在官方客户端重新登录"))?;
    Ok(email.to_string())
}

pub fn plan_name(title: &str) -> String {
    title
        .split_whitespace()
        .map(|word| {
            if word.eq_ignore_ascii_case("kiro") {
                "Kiro".into()
            } else {
                let mut chars = word.chars();
                chars
                    .next()
                    .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                    .unwrap_or_default()
            }
        })
        .collect::<Vec<String>>()
        .join(" ")
}
fn ms(value: Option<&Value>) -> Option<i64> {
    util::number(value)
        .filter(|number| *number > 0.0 && *number < 9_223_372_036_854_000.0)
        .map(|seconds| (seconds as i64).saturating_mul(1000))
}
fn short(value: f64) -> String {
    let text = format!("{value:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}
fn row(name: &str, used: Option<f64>, limit: Option<f64>, aside: bool) -> (Value, Value) {
    let left = used.zip(limit).map(|(used, limit)| (limit - used).max(0.0));
    let percent = used
        .zip(limit)
        .and_then(|(used, limit)| (limit > 0.0).then(|| (used / limit * 100.0).clamp(0.0, 100.0)));
    let display = match (used, limit) {
        (Some(used), Some(limit)) => format!("{} / {}", short(used), short(limit)),
        (Some(used), None) => format!("已用 {} / 总量未知", short(used)),
        (None, Some(limit)) => format!("已用未知 / {}", short(limit)),
        _ => "暂无额度读数".into(),
    };
    (
        json!({ "name": name, "label": name, "unit": "credits", "amount": used, "limit": limit,
        "used": percent, "remainingPercent": percent.map(|value| 100.0 - value), "remaining": left,
        "display": display, "aside": aside }),
        json!({ "name": name, "unit": "credits", "used": used, "total": limit,
        "remain": left, "remainingPercent": percent.map(|value| 100.0 - value) }),
    )
}
pub fn report(payload: &Value) -> Result<Value> {
    let breakdowns = payload["usageBreakdownList"]
        .as_array()
        .ok_or_else(|| Error::http(502, "Kiro 额度响应缺少分窗口读数"))?;
    let now = util::now()?;
    let mut windows = Vec::new();
    let mut credits = Vec::new();
    for breakdown in breakdowns {
        if let Some(trial) = breakdown
            .get("freeTrialInfo")
            .filter(|value| value.is_object())
        {
            if trial["freeTrialStatus"]
                .as_str()
                .is_some_and(|status| status.eq_ignore_ascii_case("ACTIVE"))
            {
                let (mut window, mut credit) = row(
                    "Free trial",
                    util::number(trial.get("currentUsageWithPrecision")),
                    util::number(trial.get("usageLimitWithPrecision")),
                    false,
                );
                if let Some(at) = ms(trial.get("freeTrialExpiry")) {
                    window["resetsAt"] = json!(at);
                    credit["expireAt"] = json!(at);
                }
                windows.push(window);
                credits.push(credit);
            }
        }
        let mut bonuses = Vec::new();
        for bonus in breakdown["bonuses"].as_array().into_iter().flatten() {
            if !bonus["status"]
                .as_str()
                .is_some_and(|status| status.eq_ignore_ascii_case("ACTIVE"))
            {
                continue;
            }
            let expiry = ms(bonus.get("expiresAt"));
            if expiry.is_some_and(|at| at <= now) {
                continue;
            }
            let name = bonus["displayName"]
                .as_str()
                .filter(|name| !name.is_empty())
                .unwrap_or("Bonus credits");
            let (mut window, mut credit) = row(
                name,
                util::number(bonus.get("currentUsage")),
                util::number(bonus.get("usageLimit")),
                true,
            );
            if let Some(at) = expiry {
                credit["expireAt"] = json!(at);
                if window["remaining"].as_f64().is_some_and(|left| left > 0.0) {
                    window["runsOutAt"] = json!(at);
                    window["runsOut"] = window["remaining"].clone();
                }
            }
            bonuses.push(window);
            credits.push(credit);
        }
        let used = util::number(breakdown.get("currentUsageWithPrecision"));
        let limit = util::number(breakdown.get("usageLimitWithPrecision"));
        if used.is_some() || limit.is_some() {
            let name = breakdown["displayNamePlural"]
                .as_str()
                .filter(|name| !name.is_empty())
                .or_else(|| breakdown["displayName"].as_str())
                .unwrap_or("Credits");
            let carried = used.zip(limit).is_some_and(|(used, limit)| used >= limit)
                && bonuses
                    .iter()
                    .any(|bonus| bonus["remaining"].as_f64().is_some_and(|left| left > 0.0));
            let (mut window, mut credit) = row(name, used, limit, carried);
            window["span"] = json!(30 * 24 * 3600);
            if let Some(at) =
                ms(breakdown.get("nextDateReset")).or_else(|| ms(payload.get("nextDateReset")))
            {
                window["resetsAt"] = json!(at);
                credit["resetAt"] = json!(at);
                credit["rolling"] = json!(true);
            }
            windows.push(window);
            credits.push(credit);
        }
        windows.extend(bonuses);
    }
    let mut report = json!({ "provider": endpoints::ID, "unit": "credits", "windows": windows, "credits": credits });
    let title = payload["subscriptionInfo"]["subscriptionTitle"]
        .as_str()
        .unwrap_or("");
    let email = payload["userInfo"]["email"].as_str().unwrap_or("");
    if !title.is_empty() {
        report["plan"] = json!(plan_name(title));
    }
    if !email.is_empty() {
        report["user"] = json!(email);
    }
    Ok(report)
}
pub fn query() -> Result<Value> {
    let mut session = refresh::ready()?;
    let payload = match fetch(&session.credentials) {
        Err(error) if error.status == Some(401) && !session.credentials.api_key() => {
            session = refresh::after_rejection(&session.credentials)?;
            fetch(&session.credentials)?
        }
        result => result?,
    };
    Ok(errors::sanitize(
        &report(&payload)?,
        &session.credentials.secrets(),
    ))
}
