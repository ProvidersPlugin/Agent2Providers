//! 积分单位沿用原生：amount 是百积分，points_entitlement 已是积分。
//! 未识别的余额不写成0；只保留可展示字段，不透传原始账号资料。

use super::{client, endpoints, refresh};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn shape(data: &Value) -> Result<Value> {
    if !data.is_object() {
        return Err(Error::http(502, "QuWork 积分返回格式异常"));
    }
    let entitlement = &data["points_entitlement"];
    let available = client::number(&entitlement["total_points"]).or_else(|| {
        client::number(&data["amount"])
            .map(|amount| (amount * 100.0).round())
            .filter(|value| value.is_finite())
    });
    let mut credits = Vec::new();
    for (id, name, field) in [
        ("limited", "限时积分", "limited_points"),
        ("permanent", "永久积分", "permanent_points"),
    ] {
        if let Some(remaining) = client::number(&entitlement[field]) {
            credits.push(
                json!({ "id": id, "name": name, "balance": remaining, "remain": remaining,
                "unit": "积分", "snapshot": true }),
            );
        }
    }
    let subscription = entitlement["active_subscription"].as_object();
    let plan = subscription
        .and_then(|value| value.get("plan_name"))
        .and_then(Value::as_str);
    let until = subscription
        .and_then(|value| value.get("expires_at"))
        .and_then(client::timestamp);
    if let Some(subscription) = subscription {
        let get = |key: &str| subscription.get(key).unwrap_or(&Value::Null);
        credits.push(json!({ "id": "subscription", "name": plan.unwrap_or("订阅积分"),
            "remain": client::number(get("remaining_points")), "total": client::number(get("total_points")),
            "used": client::number(get("used_points")), "grantAt": client::timestamp(get("started_at")),
            "expireAt": until, "unit": "积分" }));
    }
    let has_reading = available.is_some()
        || credits.iter().any(|row| {
            ["remain", "total", "used"]
                .iter()
                .any(|key| client::number(&row[*key]).is_some())
        });
    if !has_reading {
        return Err(Error::http(
            502,
            "QuWork 未返回可识别的积分余额，未将未知余额当作0",
        ));
    }
    let mut value = json!({ "unit": "积分", "plan": plan, "credits": credits, "until": until });
    if let Some(available) = available {
        value["available"] = json!(available);
    }
    value["quotaSources"] = sources(&value);
    Ok(value)
}

pub fn sources(report: &Value) -> Value {
    match client::number(&report["available"]) {
        Some(remaining) => {
            json!([{ "id": "account", "label": "账号积分", "provider": "quwork", "unit": "积分",
            "remaining": remaining, "total": null, "used": null }])
        }
        None => json!([]),
    }
}

pub fn query() -> Result<Value> {
    shape(&refresh::protected("GET", endpoints::SELF, None)?.data("积分查询")?)
}
