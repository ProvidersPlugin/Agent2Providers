//! 官方只读账单读数：订阅用量单位与额外美元余额分开，缺失读数不补零或猜月上限。
use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};

use super::{account, config::Config, errors};

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
        .filter(|value| value.is_finite() && *value >= 0.0 && *value <= 1e12)
}

fn request(config: &Config, key: &str, path: &str, timeout: u64) -> Result<Value> {
    let response =
        http::open_timeout("GET", &config.url(path), config.headers(key), None, timeout)?;
    let status = response.status;
    let bytes = response.bytes(1024 * 1024)?;
    let payload: Value = serde_json::from_slice(&bytes)
        .map_err(|_| Error::http(502, "Command Code 账单未返回有效 JSON"))?;
    if !(200..300).contains(&status) {
        return Err(errors::http_error(status, &payload));
    }
    if payload["success"] == false || payload.get("error").is_some_and(|error| !error.is_null()) {
        return Err(errors::http_error(502, &payload));
    }
    Ok(payload)
}

fn window(name: &str, key: &str, value: &Value) -> Option<Value> {
    let used = number(&value["used"]);
    let limit = number(&value["cap"]);
    if used.is_none() && limit.is_none() {
        return None;
    }
    let remaining = used.zip(limit).map(|(used, limit)| (limit - used).max(0.0));
    let percent = remaining
        .zip(limit)
        .filter(|(_, limit)| *limit > 0.0)
        .map(|(remaining, limit)| (remaining / limit * 100.0).clamp(0.0, 100.0));
    Some(
        json!({ "label": name, "window": key, "key": key, "unit": "credits", "amount": used,
        "remaining": remaining, "limit": limit, "remainingPercent": percent,
        "resetsAt": errors::timestamp(&value["resetAt"]) }),
    )
}

fn plan_name(id: &str) -> String {
    let id = id.to_ascii_lowercase().replace('_', "-");
    for (prefix, name) in [
        ("individual-provider", "Provider"),
        ("individual-goat", "GOAT"),
        ("individual-ultra", "Ultra"),
        ("individual-max", "Max"),
        ("individual-pro", "Pro"),
        ("individual-go", "Go"),
        ("teams-pro", "Teams Pro"),
    ] {
        if id.starts_with(prefix) {
            return name.into();
        }
    }
    String::new()
}

pub fn query() -> Result<Value> {
    let record = sdk::account()?;
    let key = account::api_key(&record["secret"])?;
    let config = Config::load()?;
    let payload =
        request(&config, &key, "/alpha/billing/credits", 10_000).map_err(|mut error| {
            errors::redact(&mut error, &key);
            error
        })?;
    account::unchanged(&record)?;
    if !payload["credits"].is_object() && !payload["windowLimits"].is_object() {
        return Err(Error::http(
            502,
            "Command Code 账单缺少余额与用量窗口，未当成零余额",
        ));
    }
    let subscription = request(&config, &key, "/alpha/billing/subscriptions", 5_000);
    account::unchanged(&record)?;
    let mut warnings = Vec::new();
    let subscription = match subscription {
        Ok(value) => value["data"].clone(),
        Err(error) if errors::host_failure(&error) && error.code.as_deref() != Some("timeout") => {
            return Err(error)
        }
        Err(error) => {
            warnings.push(errors::redact_text(
                &format!("套餐暂未读取：{}", error.message),
                &key,
            ));
            Value::Null
        }
    };
    let mut windows = Vec::new();
    for (key, name, label) in [
        ("fiveHour", "5h", "5 hours"),
        ("weekly", "weekly", "Weekly"),
    ] {
        if let Some(window) = window(label, name, &payload["windowLimits"][key]) {
            windows.push(window);
        }
    }
    let credits = &payload["credits"];
    let plan = credits["planId"]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .or_else(|| subscription["planId"].as_str())
        .unwrap_or("");
    let lower = plan.to_ascii_lowercase().replace('_', "-");
    let monthly_unit = if lower.starts_with("individual-go")
        || lower.starts_with("individual-pro") && !lower.starts_with("individual-provider")
    {
        "credits"
    } else if lower.starts_with("individual-max")
        || lower.starts_with("individual-ultra")
        || lower.starts_with("teams-")
    {
        "USD"
    } else {
        "credits"
    };
    if let Some(remaining) = number(&credits["monthlyCredits"]) {
        // 官方只返回剩余值时不从历史套餐价格造月度上限。
        windows.push(
            json!({ "label": "Monthly credits", "window": "monthly", "key": "monthly",
            "unit": monthly_unit, "remaining": remaining, "limit": null, "remainingPercent": null,
            "resetsAt": errors::timestamp(&subscription["currentPeriodEnd"]) }),
        );
    }
    for (field, name) in [
        ("purchasedCredits", "Purchased credits"),
        ("freeCredits", "Free credits"),
    ] {
        if let Some(remaining) = number(&credits[field]) {
            windows.push(json!({ "label": name, "window": name, "key": field, "unit": "USD", "remaining": remaining,
                "limit": null, "remainingPercent": null, "aside": true }));
        }
    }
    if windows.is_empty() {
        return Err(Error::http(
            502,
            "Command Code 未返回可用额度读数，未假报零余额",
        ));
    }
    let mut result =
        json!({ "provider": "commandcode", "unit": "%", "windows": windows, "warnings": warnings });
    let name = plan_name(plan);
    if !name.is_empty() {
        result["plan"] = json!(name);
        result["subscription"] = json!({ "planName": name, "status": subscription["status"].as_str().unwrap_or(""),
            "expiresAt": errors::timestamp(&subscription["currentPeriodEnd"]),
            "renew": subscription["cancelAtPeriodEnd"].as_bool().map(|cancel| if cancel { "off" } else { "auto" }) });
    }
    Ok(result)
}
