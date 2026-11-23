use agent2api_wasm_sdk::{json, Error, Result, Value};

use crate::{account, endpoints, util};

pub fn overview(current: &account::Snapshot) -> Result<Value> {
    let value = current.web("GET", endpoints::QUOTA, None)?;
    if !value.is_object() {
        return Err(Error::http(502, "百度搭子积分查询没有返回有效对象"));
    }
    Ok(value)
}

fn amounts(value: &Value) -> (Option<f64>, Option<f64>, Option<f64>) {
    let total = util::number(&value["totalPoints"]).filter(|number| *number >= 0.0);
    let used = util::number(&value["usedPoints"]).filter(|number| *number >= 0.0);
    let remaining = total.zip(used).map(|(total, used)| (total - used).max(0.0));
    (total, used, remaining)
}

pub fn packages(value: &Value, protected: &[String]) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    for (key, label) in [
        ("subscription", "订阅积分"),
        ("incremental", "赠送 / 增量积分"),
    ] {
        let Some(list) = value.get(key).filter(|list| !list.is_null()) else {
            continue;
        };
        let list = list
            .as_array()
            .filter(|list| list.len() <= 2048)
            .ok_or_else(|| Error::http(502, "百度搭子积分包格式异常"))?;
        for (index, package) in list.iter().enumerate() {
            if !package.is_object() {
                return Err(Error::http(502, "百度搭子积分包不是有效对象"));
            }
            if package
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| status != "active")
            {
                continue;
            }
            let (total, used, remaining) = amounts(package);
            let source = util::safe_text(&util::text(package, &["source"]), protected, 100);
            rows.push(json!({ "id": format!("{key}:{index}"), "kind": key, "label": label,
                "packageType": util::safe_text(&util::text(package, &["packageType"]), protected, 100),
                "source": source, "total": total, "used": used, "remaining": remaining,
                "grantedAt": util::timestamp(&package["startDate"]),
                "expiresAt": util::timestamp(&package["expireDate"]), "unit": "积分" }));
        }
    }
    Ok(rows)
}

pub fn sources(report: &Value) -> Value {
    let source = report["windows"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|window| window["name"] == "搭子积分");
    match source {
        Some(window) if util::number(&window["remaining"]).is_some() => json!([{
            "id": "points", "provider": endpoints::PROVIDER, "label": "搭子积分",
            "remaining": window["remaining"], "total": window["total"], "used": window["used"],
            "unit": "积分", "unlimited": false
        }]),
        _ => json!([]),
    }
}

pub fn query() -> Result<Value> {
    let current = account::current()?;
    let data = overview(&current)?;
    let protected = current.protected();
    let rows = packages(&data, &protected)?;
    let now = util::now()?;
    let packages = rows
        .into_iter()
        .filter(|package| {
            package["expiresAt"]
                .as_i64()
                .is_none_or(|expires| expires > now)
        })
        .collect::<Vec<_>>();
    let (total, used, remaining) = amounts(&data);
    let mut windows = Vec::new();
    let mut window = json!({ "name": "搭子积分", "label": "搭子积分", "total": total,
        "used": used, "remaining": remaining, "balance": remaining,
        "display": remaining.map(util::display), "unit": "积分", "aside": false });
    let first_expiry = packages
        .iter()
        .filter(|package| {
            package["remaining"]
                .as_f64()
                .is_some_and(|remaining| remaining > 0.0)
        })
        .filter_map(|package| package["expiresAt"].as_i64())
        .filter(|expires| *expires > now)
        .min();
    if let Some(expires) = first_expiry {
        let expiring = packages
            .iter()
            .filter(|package| package["expiresAt"].as_i64() == Some(expires))
            .filter_map(|package| package["remaining"].as_f64())
            .sum::<f64>();
        window["runsOut"] = json!(expiring);
        window["runsOutAt"] = json!(expires);
    }
    windows.push(window);
    let mut report = json!({ "provider": endpoints::PROVIDER,
        "plan": match data["isSubscribed"].as_bool() { Some(true) => "搭子订阅账号", Some(false) => "搭子体验账号", None => "百度搭子" },
        "unit": "积分", "windows": windows, "packages": packages,
        "throttled": data["modelThrottleInfo"]["throttled"].as_bool(),
        "message": "积分包和到期时间来自官方接口；缺少读数时留空，不估算余额。" });
    if remaining.is_none() {
        report["error"] = json!("百度搭子接口未返回完整的总积分与已用积分，剩余积分未知");
    }
    report["quotaSources"] = sources(&report);
    Ok(report)
}

pub fn records(payload: &Value, charge: bool) -> Result<Value> {
    let current = account::current()?;
    let now = util::now()?;
    let start = util::timestamp(&payload["startAt"]).unwrap_or(now - 30 * 86_400_000);
    let end = util::timestamp(&payload["endAt"]).unwrap_or(now);
    if start >= end || end - start > 366 * 86_400_000 {
        return Err(Error::http(400, "积分明细时间范围必须在一年以内"));
    }
    let page = payload["page"].as_u64().unwrap_or(1).clamp(1, 1000);
    let limit = payload["limit"].as_u64().unwrap_or(20).clamp(1, 100);
    let path = format!(
        "/api/dumate/points/records/{}?startAt={}&endAt={}&page={page}&limit={limit}",
        if charge { "charge" } else { "usage" },
        start / 1000,
        end / 1000
    );
    current.web("GET", &path, None)
}
