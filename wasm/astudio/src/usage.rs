//! 两个积分池分别展示，不把缺读数写成0；网页会话失效必须告知，不能影响模型凭据身份。
//! 只有模型key时保留原生目录探活，不伪造积分或会员。

use super::{account, client, endpoints, models};
use agent2api_wasm_sdk::{json, Error, Result, Value};
use chrono::TimeZone;

pub struct Balance {
    pub points: Option<f64>,
    pub spark: Option<f64>,
}

pub fn balance(secret: &Value) -> Result<Balance> {
    let data = client::studio("GET", endpoints::BALANCE, Some(secret))?.data(true, "session")?;
    let points = account::number(&data["totalBalance"]).filter(|value| *value >= 0.0);
    let spark = account::number(&data["sparkTotalBalance"]).filter(|value| *value >= 0.0);
    if points.is_none() && spark.is_none() {
        return Err(Error::http(502, "AStudio 积分余额读数不完整"));
    }
    Ok(Balance { points, spark })
}

fn timestamp(value: &Value) -> Option<Value> {
    if let Some(number) = account::number(value).filter(|number| *number > 0.0) {
        let millis = if number < 100_000_000_000.0 {
            number * 1000.0
        } else {
            number
        };
        return (millis < i64::MAX as f64).then(|| json!(millis as i64));
    }
    let text = value.as_str()?.trim();
    if text.len() > 80 || text.chars().any(char::is_control) {
        return None;
    }
    if let Ok(date) = chrono::DateTime::parse_from_rfc3339(text) {
        return (date.timestamp_millis() > 0).then(|| json!(date.timestamp_millis()));
    }
    let zone = chrono::FixedOffset::east_opt(8 * 3600)?;
    for format in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(date) = chrono::NaiveDateTime::parse_from_str(text, format) {
            let at = zone.from_local_datetime(&date).single()?.timestamp_millis();
            return (at > 0).then(|| json!(at));
        }
    }
    let date = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)?;
    let at = zone.from_local_datetime(&date).single()?.timestamp_millis();
    (at > 0).then(|| json!(at))
}

fn display(value: f64) -> String {
    if value.fract().abs() < 0.005 {
        format!("{}", value.round() as i64)
    } else {
        format!("{value:.2}")
    }
}

pub fn sources(report: &Value) -> Value {
    let mut sources = Vec::new();
    for window in report["windows"].as_array().into_iter().flatten() {
        let Some(remaining) = account::number(&window["remaining"]) else {
            continue;
        };
        let id = match window["name"].as_str() {
            Some("通用积分") => "points",
            Some("星火积分") => "spark",
            _ => continue,
        };
        sources.push(json!({ "id": id, "provider": crate::PROVIDER_ID, "label": window["name"],
            "remaining": remaining, "total": null, "used": null, "unit": "积分", "unlimited": false }));
    }
    json!(sources)
}

pub fn query() -> Result<Value> {
    let record = account::current()?;
    let secret = &record["secret"];
    if !account::has_session(secret) {
        return match models::fetch(&account::catalog_key(secret)?) {
            Ok(_) => Ok(
                json!({ "provider": crate::PROVIDER_ID, "plan": "AStudio", "windows": [],
                "reachable": true, "message": "此账号只有模型凭据，未提供积分与签到所需的网页会话" }),
            ),
            Err(error) if error.code.as_deref() == Some("astudio_model_expired") => Err(error),
            Err(error) => Ok(
                json!({ "provider": crate::PROVIDER_ID, "plan": "AStudio", "windows": [],
                "reachable": false, "error": error.message }),
            ),
        };
    }
    let mut failures = Vec::new();
    let balance = match client::studio("GET", endpoints::BALANCE, Some(secret))
        .and_then(|reply| reply.data(true, "session"))
    {
        Ok(data) => Some(data),
        Err(error) if error.code.as_deref() == Some("astudio_session_expired") => {
            return Err(error)
        }
        Err(error) => {
            failures.push(error.message);
            None
        }
    };
    let membership = match client::studio("GET", endpoints::MEMBERSHIP, Some(secret))
        .and_then(|reply| reply.data(true, "session"))
    {
        Ok(data) => Some(data),
        Err(error) if error.code.as_deref() == Some("astudio_session_expired") => {
            return Err(error)
        }
        Err(error) => {
            failures.push(error.message);
            None
        }
    };
    account::ensure_current(&record)?;
    let mut windows = Vec::new();
    if let Some(balance) = balance {
        for (field, label) in [
            ("totalBalance", "通用积分"),
            ("sparkTotalBalance", "星火积分"),
        ] {
            let Some(number) = account::number(&balance[field]).filter(|value| *value >= 0.0)
            else {
                continue;
            };
            let mut window = json!({ "name": label, "label": label, "balance": number, "remaining": number,
                "display": display(number), "unit": "积分", "aside": false });
            if field == "totalBalance" {
                if let Some(activity) =
                    account::number(&balance["activityBalance"]).filter(|value| *value > 0.0)
                {
                    if let Some(at) = timestamp(&balance["activityNextExpireTime"]) {
                        window["runsOut"] = json!(activity);
                        window["runsOutAt"] = at;
                    }
                }
            }
            windows.push(window);
        }
        if windows.is_empty() {
            failures.push("AStudio 积分读数不完整".into());
        }
    }
    let mut report = json!({ "provider": crate::PROVIDER_ID, "unit": "积分", "plan": "AStudio", "windows": windows });
    if let Some(membership) = membership {
        let plan = account::text(&membership, &["planName"]);
        if !plan.is_empty() {
            report["plan"] = json!(account::safe_text(&plan, 128));
        }
        let uid = account::scalar(&membership["uid"]);
        if !uid.is_empty() {
            report["user"] = json!(account::safe_text(&uid, 256));
        }
        if let Some(at) = timestamp(&membership["endTime"]) {
            report["until"] = at;
        }
    }
    if !failures.is_empty() {
        report["error"] = json!(failures.join("；"));
    }
    report["quotaSources"] = sources(&report);
    Ok(report)
}
