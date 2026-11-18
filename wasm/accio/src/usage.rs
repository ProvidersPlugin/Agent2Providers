//! 原生 Accio 双读数：quota 百分比窗口 + 可选订阅积分。不把国内 600 分套到国际。
//! 未返回的积分为空，不能补 0；来源 id/单位分开，修正旧钱包循环把积分也当百分比的行为。

use crate::{endpoints, errors, network, refresh, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

fn number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str()?.trim().parse().ok())
        })
        .filter(|value| value.is_finite())
}

fn compact(value: f64) -> String {
    if value >= 10_000.0 {
        format!("{:.1} 万", value / 10_000.0)
    } else if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

fn duration(value: f64) -> String {
    let total = value.max(0.0) as i64;
    if total >= 86_400 {
        format!("{} 天 {} 小时", total / 86_400, (total % 86_400) / 3600)
    } else if total >= 3600 {
        format!("{} 小时 {} 分", total / 3600, (total % 3600) / 60)
    } else if total >= 60 {
        format!("{} 分", total / 60)
    } else {
        format!("{total} 秒")
    }
}

fn subscription(value: &Value) -> Value {
    let read = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| number(value.get(*key)))
            .filter(|value| *value >= 0.0)
    };
    let expires = ["expireAt", "expiresAt", "endTime", "end_time"]
        .iter()
        .find_map(|key| util::timestamp(value.get(*key)));
    json!({ "planName": util::text(value, &["planName", "plan_name", "productName", "name"]),
        "status": util::text(value, &["status", "state"]), "expireAt": expires,
        "remainQuota": read(&["remainQuota", "remain", "remaining", "leftQuota"]),
        "totalQuota": read(&["totalQuota", "total", "quota"]), "entitlement": buckets(value) })
}

fn buckets(value: &Value) -> Value {
    let mut result = json!({});
    for key in ["daily", "monthly", "referral"] {
        let bucket = &value["entitlement"][key];
        let Some(remaining) = number(bucket.get("remaining")).filter(|value| *value >= 0.0) else {
            continue;
        };
        result[key] = json!({ "remaining": remaining,
            "total": number(bucket.get("total")).filter(|value| *value >= 0.0),
            "used": number(bucket.get("used")).filter(|value| *value >= 0.0) });
    }
    result
}

fn read(session: &refresh::Session) -> Result<Value> {
    refresh::check(session)?;
    let credential = &session.credentials;
    let quota_result = network::get(credential, endpoints::QUOTA);
    let quota = match &quota_result {
        Ok(value) => network::data(value.clone()),
        Err(error) if errors::host_stop(error) || error.status == Some(401) => {
            return Err(error.clone())
        }
        Err(_) => Value::Null,
    };
    refresh::check(session)?;
    let percent = number(
        quota
            .get("usagePercent")
            .or_else(|| quota.get("usage_percent")),
    )
    .filter(|value| (0.0..=100.0).contains(value));
    let remaining = percent.map(|percent| 100.0 - percent);
    let countdown = number(
        quota
            .get("refreshCountdownSeconds")
            .or_else(|| quota.get("refresh_countdown_seconds")),
    )
    .filter(|value| *value >= 0.0 && *value <= 31_536_000.0);
    let expires = countdown
        .map(|seconds| util::now().map(|now| now.saturating_add((seconds * 1000.0) as i64)))
        .transpose()?;
    let subscription = match network::get(credential, endpoints::SUBSCRIPTION) {
        Ok(value) => subscription(&network::data(value)),
        Err(error) if errors::host_stop(&error) || error.status == Some(401) => return Err(error),
        // 免费账号可没有订阅，但不能伪造积分读数。
        Err(_) => Value::Null,
    };
    refresh::check(session)?;
    let mut wallet = json!({ "type": "entitlement", "displayName": "额度余量", "balance": remaining,
        "unit": "%", "usedPercent": percent, "resetsAt": expires });
    if let Some(seconds) = countdown.filter(|value| *value > 0.0) {
        wallet["detail"] = json!(format!("{} 后重置", duration(seconds)));
    }
    let mut wallets = if percent.is_some() {
        vec![wallet]
    } else {
        Vec::new()
    };
    for (key, label) in [
        ("daily", "每日积分"),
        ("monthly", "每月积分"),
        ("referral", "推荐积分"),
    ] {
        let bucket = &subscription["entitlement"][key];
        let Some(balance) = number(bucket.get("remaining")) else {
            continue;
        };
        let total = number(bucket.get("total")).filter(|value| *value > 0.0);
        let used =
            number(bucket.get("used")).or_else(|| total.map(|total| (total - balance).max(0.0)));
        let mut wallet = json!({ "type": "credits", "sourceId": key, "displayName": label,
            "balance": balance, "unit": "积分", "total": total, "used": used });
        if let Some((total, used)) = total.zip(used) {
            wallet["usedPercent"] = json!((used / total * 100.0).clamp(0.0, 100.0));
        }
        if key == "daily" {
            wallet["resetsAt"] = json!(expires);
        }
        wallets.push(wallet);
    }
    let credits = number(subscription.get("remainQuota")).filter(|value| *value >= 0.0);
    if let Some(credits) =
        credits.filter(|_| !wallets.iter().any(|wallet| wallet["type"] == "credits"))
    {
        let total = number(subscription.get("totalQuota")).filter(|total| *total > 0.0);
        let mut wallet =
            json!({ "type": "credits", "displayName": "积分", "balance": credits, "unit": "积分" });
        if let Some(total) = total {
            wallet["total"] = json!(total);
            wallet["usedPercent"] = json!(((total - credits) / total * 100.0).clamp(0.0, 100.0));
        }
        wallets.push(wallet);
    }
    let credits = credits.or_else(|| {
        let values: Vec<f64> = wallets
            .iter()
            .filter(|wallet| wallet["type"] == "credits")
            .filter_map(|wallet| number(wallet.get("balance")))
            .collect();
        (!values.is_empty()).then(|| values.iter().sum::<f64>())
    });
    if wallets.is_empty() {
        return Err(quota_result.err().unwrap_or_else(|| {
            Error::http(
                502,
                "Accio 额度未返回有效用量或积分，不能把未知剩余量显示为零",
            )
        }));
    }
    let view = match (remaining, credits) {
        (Some(remaining), Some(credits)) => format!("{remaining:.0}% · {} 积分", compact(credits)),
        (Some(remaining), None) => format!("{remaining:.0}%"),
        (None, Some(credits)) => format!("{} 积分", compact(credits)),
        _ => String::new(),
    };
    let mut report = json!({ "available": null, "availableView": view,
        "unit": if percent.is_some() { "%" } else { "积分" }, "wallets": wallets, "subscription": subscription,
        "raw": { "quota": { "usagePercent": percent, "refreshCountdownSeconds": countdown }, "region": endpoints::EDITION, "account": credential.user } });
    report["quotaSources"] = sources(&report);
    Ok(errors::sanitize(&report, &credential.secrets()))
}

pub fn query() -> Result<Value> {
    // 余额本身先用只读凭据；明确 401 才续期一次，防止每次查看余额都消耗刷新链。
    let session = refresh::snapshot()?;
    match read(&session) {
        Err(error) if error.status == Some(401) && !session.credentials.refresh.is_empty() => {
            read(&refresh::after_rejection(&session.credentials)?)
        }
        result => result,
    }
}

pub fn sources(report: &Value) -> Value {
    let mut sources = Vec::new();
    for wallet in report["wallets"].as_array().into_iter().flatten() {
        match wallet["type"].as_str() {
            Some("entitlement") => sources.push(json!({ "provider": endpoints::ID, "id": "entitlement", "label": "额度余量",
                "unit": "%", "remainingPercent": wallet["balance"], "resetsAt": wallet["resetsAt"] })),
            Some("credits") => sources.push(json!({ "provider": endpoints::ID,
                "id": wallet["sourceId"].as_str().unwrap_or("credits"),
                "label": wallet["displayName"].as_str().unwrap_or("积分"), "unit": "积分",
                "remaining": wallet["balance"], "total": wallet.get("total"), "resetsAt": wallet.get("resetsAt"),
                "used": number(wallet.get("used")).or_else(|| number(wallet.get("total"))
                    .zip(number(wallet.get("balance"))).map(|(total, remaining)| (total - remaining).max(0.0))) })),
            _ => {}
        }
    }
    json!(sources)
}
