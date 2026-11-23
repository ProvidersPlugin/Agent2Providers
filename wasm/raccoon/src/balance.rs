//! 余额以total_chargeable_points优先，五钱包含企业加油包；订阅失败不抹掉积分。
//! 每日积分是按天发放的余额，不能宣称每日清零；未知读数不写零。
//! 上游 /points/v1/balance 受限(403等非401)时，回退读取 setting_info 的 available_points。

use super::{config, credentials::Credential, network, session, util};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

const COOLDOWN_KEY: &str = "points-balance:cooldown";
const COOLDOWN_TTL_MS: i64 = 180_000;

const WALLETS: &[(&str, &str)] = &[
    ("daily_points", "每日积分"),
    ("monthly_points", "每月会员积分"),
    ("reward_points", "奖励积分"),
    ("topup_points", "充值积分"),
    ("enterprise_topup_points", "企业加油包积分"),
];

fn control_headers() -> Result<Value> {
    let mut headers = config::page(false)?;
    headers["Accept"] = json!("application/json");
    headers["X-Raccoon-Language"] = json!("zh");
    Ok(headers)
}

fn available_points(value: &Value) -> Option<i64> {
    util::integer(&value["total_chargeable_points"]).or_else(|| util::integer(&value["available_points"]))
}

fn fetch_points(credential: &Credential) -> Result<(Value, bool, Option<String>)> {
    let in_cooldown = sdk::session_get(COOLDOWN_KEY)
        .ok()
        .is_some_and(|value| value == true);
    let mut first_error = None;
    if !in_cooldown {
        match network::request(
            "GET",
            "/api/web/points/v1/balance",
            &credential.token,
            &credential.org,
            control_headers()?,
            None,
        ) {
            Ok(points) if points.is_object() && available_points(&points).is_some() => {
                return Ok((points, false, None));
            }
            Ok(points) => {
                first_error = Some(Error::http(
                    502,
                    if points.is_object() {
                        "积分余额响应缺少可用积分"
                    } else {
                        "积分余额响应不是JSON对象"
                    },
                ));
            }
            Err(error) => {
                if matches!(error.status, Some(401 | 409 | 503)) {
                    return Err(error);
                }
                if error.code.as_deref() == Some("raccoon_edge_403") {
                    let _ = sdk::session_put(COOLDOWN_KEY, json!(true), COOLDOWN_TTL_MS);
                }
                first_error = Some(error);
            }
        }
    }
    let path = if credential.org.is_empty() {
        "/api/web/office/v3/setting_info"
    } else {
        "/api/web/org/user/office/v1/setting_info"
    };
    let info = match network::request(
        "GET",
        path,
        &credential.token,
        &credential.org,
        control_headers()?,
        None,
    ) {
        Ok(value) => value,
        Err(error) => {
            if error.status == Some(401) {
                return Err(error);
            }
            if in_cooldown {
                let _ = sdk::session_remove(COOLDOWN_KEY);
                return Err(error);
            }
            return Err(first_error.unwrap_or(error));
        }
    };
    if !info.is_object() || available_points(&info).is_none() {
        return Err(first_error.unwrap_or_else(|| Error::http(502, "小浣熊设置接口未返回可用积分")));
    }
    Ok((info, true, first_error.map(|error| error.message)))
}

pub fn query() -> Result<Value> {
    let session = session::ensure(false)?;
    let credential = &session.credential;
    let (points, from_setting, points_error) = fetch_points(credential)?;
    let available = available_points(&points);
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
        control_headers()?,
        None,
    );
    let mut raw = if from_setting {
        json!({ "pointsSource": "setting_info", "settingInfo": points })
    } else {
        json!({ "points": points })
    };
    if let Some(error) = points_error {
        raw["pointsError"] = json!(error);
    }
    let mut result = json!({ "available": available, "unit": "积分", "wallets": wallets, "quotaSources": sources,
        "subscription": null, "raw": raw });
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
