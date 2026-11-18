//! 余额以total_chargeable_points优先，五钱包含企业加油包；订阅失败不抹掉积分。
//! 每日积分是按天发放的余额，不能宣称每日清零；未知读数不写零。

use super::{network, session, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

const WALLETS: &[(&str, &str)] = &[
    ("daily_points", "每日积分"),
    ("monthly_points", "每月会员积分"),
    ("reward_points", "奖励积分"),
    ("topup_points", "充值积分"),
    ("enterprise_topup_points", "企业加油包积分"),
];

pub fn query() -> Result<Value> {
    let session = session::ensure(false)?;
    let credential = &session.credential;
    let points = network::request(
        "GET",
        "/api/web/points/v1/balance",
        &credential.token,
        &credential.org,
        json!({ "Accept": "application/json" }),
        None,
    )?;
    if !points.is_object() {
        return Err(Error::http(502, "积分余额响应不是JSON对象"));
    }
    let available = util::integer(&points["total_chargeable_points"])
        .or_else(|| util::integer(&points["available_points"]));
    let mut wallets = Vec::new();
    let mut credits = Vec::new();
    let mut sources = Vec::new();
    for (key, name) in WALLETS {
        if let Some(balance) = util::integer(&points[*key]) {
            wallets.push(json!({ "type": key, "displayName": name, "balance": balance }));
            credits.push(json!({ "name": name, "remain": balance }));
            sources.push(json!({ "id": format!("wallet:{key}"), "label": name, "provider": "raccoon", "unit": "积分",
                "remaining": balance, "used": null, "total": null, "unlimited": false }));
        }
    }
    let subscription = network::request(
        "GET",
        "/api/web/auth/v1/entitlement_info",
        &credential.token,
        &credential.org,
        json!({ "Accept": "application/json" }),
        None,
    );
    let mut result = json!({ "available": available, "unit": "积分", "wallets": wallets, "quotaSources": sources,
        "subscription": null, "raw": { "points": points } });
    match subscription {
        Ok(value) => {
            result["subscription"] = normalize(&value);
            result["raw"]["entitlement"] = value;
        }
        Err(error) => result["raw"]["subscriptionError"] = json!(error.message),
    }
    if !credits.is_empty() {
        result["credits"] = json!(credits);
    }
    super::ledger::check_snapshot(&session)?;
    Ok(result)
}
fn normalize(data: &Value) -> Value {
    let office = data.get("office").filter(|value| value.is_object());
    let code = data.get("code").filter(|value| value.is_object());
    if office.is_none() && code.is_none() {
        return Value::Null;
    }
    let field = |key: &str| {
        office
            .and_then(|value| value.get(key))
            .or_else(|| code.and_then(|value| value.get(key)))
            .filter(|value| !value.is_null())
    };
    let pro = office
        .and_then(|value| value["pro_enable"].as_bool())
        .or_else(|| code.and_then(|value| value["pro_enable"].as_bool()))
        .unwrap_or(false);
    let plan = field("active_plan")
        .and_then(|value| value.get("type"))
        .or_else(|| field("plan_type"));
    let expires = field("pro_expired_time")
        .or_else(|| field("active_plan").and_then(|value| value.get("subscription_expired_at")));
    json!({ "planName": plan.and_then(Value::as_str).unwrap_or(""), "status": if pro { "active" } else { "none" },
        "expireAt": expires, "remainQuota": null, "totalQuota": null })
}
pub fn sources(report: &Value) -> Value {
    report["quotaSources"]
        .as_array()
        .cloned()
        .map(Value::Array)
        .unwrap_or_else(|| json!([]))
}
