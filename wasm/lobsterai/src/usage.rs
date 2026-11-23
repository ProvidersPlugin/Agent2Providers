//! 使用 profile-summary 的总积分及积分包；/user/quota 不含活动奖励，不能替代。
//! 缺失读数不补零。仅输出白名单计量字段，原始 payload 不透传到管理界面。

use super::{account, upstream, version};
use agent2api_wasm_sdk::{json, Result, Value};
use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone};

fn credits(value: f64) -> f64 {
    let value = value.max(0.0);
    if value <= f64::MAX / 100.0 {
        (value * 100.0).round() / 100.0
    } else {
        value
    }
}

fn expires_at(value: &Value) -> Option<i64> {
    if let Some(number) = value.as_i64().filter(|number| *number > 0) {
        return Some(if number < 100_000_000_000 {
            number.saturating_mul(1000)
        } else {
            number
        });
    }
    let raw = value.as_str()?.trim();
    if raw.len() > 128 {
        return None;
    }
    if let Ok(value) = DateTime::parse_from_rfc3339(raw) {
        return Some(value.timestamp_millis());
    }
    let offset = FixedOffset::east_opt(8 * 3600)?;
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(value) = NaiveDateTime::parse_from_str(raw, format) {
            return offset
                .from_local_datetime(&value)
                .single()
                .map(|value| value.timestamp_millis());
        }
    }
    None
}

pub fn shape(payload: &Value) -> Value {
    let mut wallets = Vec::new();
    let mut details = Vec::new();
    let mut amounts = Vec::new();
    let mut complete = true;
    for item in payload["creditItems"].as_array().into_iter().flatten() {
        if !item.is_object() {
            complete = false;
            continue;
        }
        let name = item["label"]
            .as_str()
            .or_else(|| item["type"].as_str())
            .map(str::trim)
            .filter(|value| {
                !value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control)
            })
            .unwrap_or("积分包");
        let remaining = account::number(&item["creditsRemaining"]).map(credits);
        let mut wallet = json!({ "type": "credit_package", "displayName": name });
        let mut detail = json!({ "name": name });
        if let Some(remaining) = remaining {
            amounts.push(remaining);
            wallet["balance"] = json!(remaining);
            wallet["remaining"] = json!(remaining);
            detail["remain"] = json!(remaining);
        } else {
            complete = false;
        }
        if let Some(expires) = expires_at(&item["expiresAt"]).filter(|expires| *expires > 0) {
            wallet["expiresAt"] = json!(expires);
            detail["expireAt"] = json!(expires);
        }
        wallets.push(wallet);
        details.push(detail);
    }
    let total = account::number(&payload["totalCreditsRemaining"])
        .map(credits)
        .or_else(|| {
            let sum: f64 = amounts.iter().sum();
            (complete && !amounts.is_empty() && sum.is_finite()).then(|| credits(sum))
        });
    let mut report = json!({ "unit": "积分", "wallets": wallets, "credits": details });
    if let Some(total) = total {
        report["available"] = json!(total);
    }
    report["quotaSources"] = sources(&report);
    report
}

pub fn sources(report: &Value) -> Value {
    let mut sources = Vec::new();
    if let Some(remaining) = account::number(&report["available"]) {
        sources.push(json!({ "id": "account", "label": "账号主额度", "provider": "lobsterai",
            "unit": "积分", "remaining": remaining, "total": null, "used": null, "unlimited": false }));
    }
    let mut ids = std::collections::HashMap::<String, usize>::new();
    for row in report["credits"].as_array().into_iter().flatten() {
        let Some(remaining) = account::number(&row["remain"]) else {
            continue;
        };
        let name = row["name"].as_str().unwrap_or("积分包");
        let base = format!("credit:{name}:null:{}", row["expireAt"]);
        let count = ids.entry(base.clone()).or_default();
        let id = if *count == 0 {
            base
        } else {
            format!("{base}:{}", *count)
        };
        *count += 1;
        sources.push(
            json!({ "id": id, "label": name, "provider": "lobsterai", "unit": "积分",
            "remaining": remaining, "total": null, "used": null, "unlimited": false,
            "startsAt": 0, "endsAt": row["expireAt"].as_i64().unwrap_or(0), "planId": id, "planName": name }),
        );
    }
    json!(sources)
}

pub fn query() -> Result<Value> {
    let account = account::prepared()?;
    let token = account::access(&account["secret"])?;
    let version = version::resolve();
    account::validate_current(&account)?;
    let query = account::keyfrom_fields(&account["secret"], &version)
        .into_iter()
        .map(|(key, value)| format!("{key}={}", upstream::query(&value)))
        .collect::<Vec<_>>()
        .join("&");
    let payload = upstream::get(
        &format!("/api/user/profile-summary?{query}"),
        &token,
        &version,
        "积分查询",
    )?;
    account::validate_current(&account)?;
    Ok(shape(&payload))
}
