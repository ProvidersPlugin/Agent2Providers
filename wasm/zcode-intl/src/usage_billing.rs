//! 保留 billing 的按权益生效时刻判定、逐桶明细、未知额度不填零等语义。
use super::{
    account, config,
    network::{self, compact, integer, number, text},
};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

const STARTS: [&str; 6] = [
    "starts_at",
    "start_at",
    "startsAt",
    "startAt",
    "effective_at",
    "not_before",
];
const ENDS: [&str; 6] = [
    "ends_at",
    "end_at",
    "endsAt",
    "endAt",
    "expire_at",
    "expires_at",
];

pub fn fetch(record: &Value) -> Result<Value> {
    let jwt = account::jwt(&record["secret"]);
    let mid = account::mid(&record["secret"]);
    let version = config::app_version()?;
    let reply = network::request(
        "GET",
        &config::billing_url("balance", &version),
        config::billing_headers(&jwt, &mid),
        None,
    )?;
    if reply.status == 429 {
        return Err(Error::http(
            502,
            "ZCode 余额查询被上游限流，请稍后或逐个查询",
        ));
    }
    let data = network::envelope(reply, "ZCode 余额查询")?;
    if !data.is_object()
        || data
            .get("plans")
            .is_some_and(|plans| !plans.is_null() && !plans.is_array())
        || data
            .get("balances")
            .is_some_and(|balances| !balances.is_null() && !balances.is_array())
    {
        return Err(Error::http(502, "ZCode 余额响应缺少有效套餐数据"));
    }
    Ok(normalize(&data))
}

fn seconds(value: &Value) -> Option<f64> {
    number(value).map(|value| if value > 1e11 { value / 1000.0 } else { value })
}
fn bound(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| seconds(&value[*key]))
}
fn effective(plan: &Value) -> Vec<f64> {
    match plan["entitlements"]
        .as_array()
        .filter(|items| !items.is_empty())
    {
        Some(items) => items
            .iter()
            .filter_map(|item| seconds(&item["effective_at"]).filter(|value| *value > 0.0))
            .collect(),
        None => bound(plan, &STARTS)
            .filter(|value| *value > 0.0)
            .into_iter()
            .collect(),
    }
}
fn pending(plan: &Value, now: f64) -> Option<f64> {
    effective(plan)
        .into_iter()
        .filter(|at| *at > now)
        .reduce(f64::min)
}
fn state(plan: &Value, now: f64) -> &'static str {
    if bound(plan, &ENDS).is_some_and(|at| at > 0.0 && at <= now) {
        return "expired";
    }
    if pending(plan, now).is_some() {
        return "pending";
    }
    let status = text(plan, &["status"]).to_ascii_lowercase();
    if matches!(status.as_str(), "active" | "valid") {
        "active"
    } else if status.contains("expire")
        || status.contains("cancel")
        || matches!(status.as_str(), "ended" | "invalid")
    {
        "expired"
    } else {
        "unknown"
    }
}
fn usable(plan: &Value, now: f64) -> bool {
    let times = effective(plan);
    state(plan, now) != "expired" && (times.is_empty() || !times.iter().all(|at| *at > now))
}
fn name(plan: &Value) -> String {
    text(plan, &["name", "plan_id"])
}
fn rank(plan: &Value, now: f64) -> (u8, f64) {
    if let Some(end) = bound(plan, &ENDS).filter(|end| *end > now) {
        (0, end)
    } else if plan["entitlements"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|item| {
            item["period"]
                .as_str()
                .is_some_and(|period| period.eq_ignore_ascii_case("daily"))
        })
    {
        (1, 0.0)
    } else {
        (2, 0.0)
    }
}
fn representative<'a>(plans: &[&'a Value], now: f64) -> Option<&'a Value> {
    let mut pool = plans
        .iter()
        .copied()
        .filter(|plan| state(plan, now) == "active")
        .collect::<Vec<_>>();
    if pool.is_empty() {
        pool = plans
            .iter()
            .copied()
            .filter(|plan| state(plan, now) == "pending")
            .collect();
    }
    if pool.is_empty() {
        pool = plans.to_vec();
    }
    pool.into_iter().min_by(|a, b| {
        rank(a, now)
            .partial_cmp(&rank(b, now))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| text(a, &["plan_id"]).cmp(&text(b, &["plan_id"])))
    })
}
fn entitlements(plan: &Value) -> Vec<Value> {
    plan["entitlements"].as_array().into_iter().flatten().filter_map(|item| {
        let show = text(item, &["show_name"]);
        if show.is_empty() { return None; }
        Some(json!({ "showName": show, "unitType": text(item, &["unit_type"]),
            "grantUnits": integer(&item["grant_units"]).or_else(|| number(&item["grant_units"]).map(|value| value as i64)),
            "period": text(item, &["period"]), "effectiveAt": seconds(&item["effective_at"]).map(|value| value as i64),
            "usedUnits": number(&item["used_units"]), "totalUnits": number(&item["total_units"]), "remainingUnits": number(&item["remaining_units"]) }))
    }).collect()
}

pub fn normalize(data: &Value) -> Value {
    let now = seconds(&data["server_time"])
        .filter(|value| *value > 0.0)
        .unwrap_or(sdk::now() as f64 / 1000.0);
    let plans = data["plans"]
        .as_array()
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let mut wallets = Vec::new();
    let mut total = 0.0;
    let mut available = 0.0;
    let mut unit = String::new();
    let mut totals_known = true;
    let mut balances_known = true;
    let mut mixed_units = false;
    for bucket in data["balances"].as_array().into_iter().flatten() {
        let owners = plans
            .iter()
            .copied()
            .filter(|plan| {
                match (
                    bucket["user_plan_id"].as_str(),
                    plan["user_plan_id"].as_str(),
                ) {
                    (Some(left), Some(right)) => left == right,
                    _ => {
                        bucket["plan_id"].as_str().is_some() && bucket["plan_id"] == plan["plan_id"]
                    }
                }
            })
            .collect::<Vec<_>>();
        if !owners.is_empty() && owners.iter().all(|plan| !usable(plan, now)) {
            continue;
        }
        let bucket_total = number(&bucket["total_units"]);
        totals_known &= bucket_total.is_some();
        let used = number(&bucket["used_units"]);
        let remaining = number(&bucket["remaining_units"])
            .or_else(|| bucket_total.zip(used).map(|(total, used)| total - used))
            .or_else(|| number(&bucket["available_units"]));
        let Some(remaining) = remaining else {
            balances_known = false;
            totals_known = false;
            continue;
        };
        let row_unit = text(bucket, &["unit_type"]);
        if row_unit.is_empty() {
            totals_known = false;
        } else if unit.is_empty() {
            unit = row_unit.clone();
        } else if row_unit != unit {
            mixed_units = true;
        }
        let display = text(bucket, &["show_name"]);
        let view = match bucket_total.filter(|total| *total > 0.0) {
            Some(total) => format!("{} / {} {row_unit}", compact(remaining), compact(total)),
            None => format!("{} {row_unit}", compact(remaining)),
        };
        total += bucket_total.unwrap_or(0.0);
        available += remaining;
        let kind = text(bucket, &["bucket_id", "plan_id"]);
        wallets.push(json!({ "type": if kind.is_empty() { "zcode_balance" } else { &kind },
            "displayName": if display.is_empty() { "额度" } else { &display },
            "balance": remaining, "balanceView": view.trim_end(), "total": bucket_total, "used": used, "unit": row_unit,
            "remainingPercent": bucket_total.filter(|total| *total > 0.0).map(|total| (remaining / total * 100.0).clamp(0.0, 100.0)),
            "planName": owners.iter().map(|plan| name(plan)).find(|name| !name.is_empty()) }));
    }
    if mixed_units {
        unit.clear();
    }
    let can_sum = !wallets.is_empty()
        && balances_known
        && !mixed_units
        && wallets
            .iter()
            .all(|wallet| wallet["unit"].as_str().is_some_and(|unit| !unit.is_empty()));
    let mut subscription = json!({});
    if let Some(plan) = representative(&plans, now) {
        subscription["planName"] = json!(name(plan));
        subscription["state"] = json!(state(plan, now));
        if let Some(status) = plan["status"].as_str() {
            subscription["status"] = json!(status);
        }
        if let Some(end) = seconds(&plan["ends_at"]).filter(|end| *end > 0.0) {
            subscription["expireAt"] = json!((end * 1000.0) as i64);
        }
    }
    if can_sum && totals_known && total > 0.0 {
        subscription["totalQuota"] = json!(total);
        subscription["remainQuota"] = json!(available);
    }
    let plans = plans.iter().map(|plan| json!({ "planId": text(plan, &["plan_id"]), "userPlanId": plan["user_plan_id"].as_str(),
        "name": name(plan), "state": state(plan, now), "startsAt": bound(plan, &STARTS).filter(|at| *at > 0.0).map(|at| at as i64),
        "endsAt": bound(plan, &ENDS).filter(|at| *at > 0.0).map(|at| at as i64),
        "pendingUntil": pending(plan, now).map(|at| at as i64), "entitlements": entitlements(plan) })).collect::<Vec<_>>();
    json!({ "available": if can_sum { json!(available) } else { Value::Null },
        "availableView": if wallets.is_empty() { "未返回可用额度读数".to_string() }
            else if can_sum { format!("{} {unit}", compact(available)) }
            else { format!("已读取 {} 项独立额度", wallets.len()) },
        "unit": unit, "wallets": wallets, "subscription": subscription, "plans": plans,
        "source": "zcode.z.ai/billing", "quotaSources": super::usage_sources::billing(data) })
}
