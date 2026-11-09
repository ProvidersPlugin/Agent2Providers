use agent2api_wasm_sdk::{self as sdk, json, Result, Value};

pub fn parse_time(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text).ok().map(|time| time.timestamp_millis())
        .or_else(|| chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").ok()
            .and_then(|time| chrono::FixedOffset::east_opt(8 * 3600).and_then(|zone| time.and_local_timezone(zone).single()))
            .map(|time| time.timestamp_millis()))
}

pub fn active_until(status: &Value) -> Option<i64> {
    parse_time(status["codingplan_free"]["expires_at"].as_str().unwrap_or(""))
        .filter(|until| *until > sdk::now())
}

pub fn active(status: &Value) -> bool {
    let plan = &status["codingplan_free"];
    match parse_time(plan["expires_at"].as_str().unwrap_or("")) {
        Some(until) => until > sdk::now(),
        None => plan["remaining_days"].as_i64().unwrap_or(0) > 0,
    }
}

pub fn shape(status: &Value) -> Value {
    let wallets: Vec<Value> = status["rate_limit_windows"].as_array().into_iter().flatten().filter_map(|item| {
        if item["show_enable"].as_i64() != Some(1) { return None; }
        let limit = item["call_limit"].as_i64().filter(|n| *n > 0)?;
        let used = item["calls_used"].as_i64().filter(|n| *n >= 0)?;
        let hours = item["window_hours"].as_i64().unwrap_or(0);
        let remaining = if item["quota_exhausted"] == true { 0 } else { limit.saturating_sub(used).max(0) };
        Some(json!({
            "type": "rolling_window", "windowHours": hours,
            "displayName": if hours > 0 { format!("近 {hours} 小时调用次数") } else { "调用次数窗口".into() },
            "balance": remaining, "remaining": remaining, "limit": limit, "used": used,
            "resetAt": item["reset_at_display"], "exhausted": item["quota_exhausted"] == true
        }))
    }).collect();
    let mut subscription = json!({ "status": if active(status) { "active" } else { "inactive" } });
    if let Some(name) = status["codingplan_free"]["plan_name"].as_str() { subscription["planName"] = json!(name); }
    if let Some(time) = status["codingplan_free"]["expires_at"].as_str() { subscription["planExpiresAt"] = json!(time); }
    let quota_sources: Vec<Value> = wallets.iter().map(|wallet| {
        let hours = wallet["windowHours"].as_i64().unwrap_or(0);
        let mut source = json!({ "id": format!("calls:{hours}"), "provider": "atomcode",
            "label": wallet["displayName"], "unit": "次", "remaining": wallet["remaining"],
            "total": wallet["limit"], "used": wallet["used"] });
        if hours > 0 { source["window"] = json!({ "reset": "rolling", "seconds": hours.saturating_mul(3600) }); }
        source
    }).collect();
    let mut result = json!({ "unit": "次", "wallets": wallets, "subscription": subscription, "quotaSources": quota_sources });
    // 缺字段是未知，不是零余额；只有完整窗口才能参与账号限额判定。
    if let Some(first) = wallets.first() {
        result["available"] = first["remaining"].clone();
        result["subscription"]["remainQuota"] = first["remaining"].clone();
        result["subscription"]["totalQuota"] = first["limit"].clone();
    }
    result
}

pub fn query() -> Result<Value> {
    let token = super::account::ensure()?;
    Ok(shape(&super::upstream::status(&token)?))
}
