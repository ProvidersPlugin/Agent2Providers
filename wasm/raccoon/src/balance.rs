//! 余额以total_chargeable_points优先，五钱包含企业加油包；订阅失败不抹掉积分。
//! 每日积分是按天发放的余额，不能宣称每日清零；未知读数不写零。
//!
//! 2026-11上游把余额限流在`office/v2/setting_info`：旧`points/v1/balance`与
//! `office/v3/setting_info`对整个网关回403（`X-TT-System-Error: 3`，任何凭证都
//! 一样），v2仍正常返回。通道顺序因此是v2→v3→旧balance，全部失败才报错，
//! 不能把"查不到"写成0覆盖真实余额。

use super::{credentials::Credential, network, session, util};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

const SETTING_V2: &str = "/api/web/office/v2/setting_info";
const SETTING_V3: &str = "/api/web/office/v3/setting_info";
const SETTING_ORG: &str = "/api/web/org/user/office/v1/setting_info";
const LEGACY_BALANCE: &str = "/api/web/points/v1/balance";

const COOLDOWN_KEY: &str = "points-balance:cooldown";
const COOLDOWN_TTL_MS: i64 = 180_000;

const WALLETS: &[(&str, &str)] = &[
    ("daily_points", "每日积分"),
    ("monthly_points", "每月会员积分"),
    ("reward_points", "奖励积分"),
    ("topup_points", "充值积分"),
    ("enterprise_topup_points", "企业加油包积分"),
];

/// 网页版页面身份：余额主通道 `office/v2/setting_info` 就是网页版在调的接口，
/// 用桌面客户端身份（`desktop-*` + `raccoon-work`）去读会落到另一套口径。
/// 网页版版本固定为 `v1.1.3`（桌面客户端版本设置不适用于这条链路）。
fn control_headers() -> Value {
    json!({ "Accept": "application/json", "X-Client-Platform": "web",
        "X-Client-Version": "v1.1.3", "User-Agent": "raccoon-ai/1.1.3",
        "X-Raccoon-Language": "zh" })
}

fn available_points(value: &Value) -> Option<i64> {
    util::integer(&value["total_chargeable_points"]).or_else(|| util::integer(&value["available_points"]))
}

/// setting_info 的通道顺序：团队号先组织路径，之后都是个人口径。
fn setting_routes(credential: &Credential) -> Vec<(&'static str, bool)> {
    let mut routes = Vec::new();
    if !credential.org.is_empty() {
        routes.push((SETTING_ORG, true));
    }
    routes.push((SETTING_V2, false));
    routes.push((SETTING_V3, false));
    routes
}

/// 发一次 setting_info；`organization=false` 时不带 X-Org-Code（否则读到组织口径）。
fn setting_request(credential: &Credential, path: &str, organization: bool) -> Result<Value> {
    network::request(
        "GET",
        path,
        &credential.token,
        if organization { &credential.org } else { "" },
        control_headers(),
        None,
    )
}

/// 读积分：setting_info 按 v2→v3 顺序，旧 points/v1/balance 只作最后兜底。
/// 返回 `(读数, 是否来自 setting_info, 已被更早通道记下的错误)`。
fn fetch_points(credential: &Credential) -> Result<(Value, bool, Option<String>)> {
    let in_cooldown = sdk::session_get(COOLDOWN_KEY)
        .ok()
        .is_some_and(|value| value == true);
    let mut first_error = None;
    for (path, organization) in setting_routes(credential) {
        match setting_request(credential, path, organization) {
            Ok(value) if value.is_object() && available_points(&value).is_some() => {
                return Ok((value, true, first_error.map(|error: Error| error.message)));
            }
            Ok(value) => {
                first_error = Some(Error::http(
                    502,
                    if value.is_object() {
                        "小浣熊设置接口未返回可用积分"
                    } else {
                        "积分余额响应不是JSON对象"
                    },
                ));
            }
            Err(error) => {
                if matches!(error.status, Some(401 | 409 | 503)) {
                    return Err(error);
                }
                first_error = Some(error);
            }
        }
    }
    if !in_cooldown {
        match network::request(
            "GET",
            LEGACY_BALANCE,
            &credential.token,
            &credential.org,
            control_headers(),
            None,
        ) {
            Ok(points) if points.is_object() && available_points(&points).is_some() => {
                return Ok((points, false, first_error.map(|error: Error| error.message)));
            }
            Ok(_) => {
                first_error = Some(Error::http(502, "积分余额响应缺少可用积分"));
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
    Err(first_error.unwrap_or_else(|| Error::http(502, "小浣熊积分查询失败")))
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
        control_headers(),
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

#[cfg(test)]
mod channel_order {
    //! 通道顺序与 2026-11 实测的上游行为对齐：
    //! `office/v2/setting_info` 存活，`office/v3/setting_info` 与
    //! `points/v1/balance` 对整个网关回 403（`X-TT-System-Error: 3`）。
    //! 宿主网络在这里不可用，所以只钉住「按什么顺序、带不带组织头」这层。

    use super::{setting_routes, Credential, SETTING_ORG, SETTING_V2, SETTING_V3};

    fn credential(org: &str) -> Credential {
        Credential {
            token: "stub-token".to_string(),
            refresh: String::new(),
            expires: None,
            user: "user-1".to_string(),
            org: org.to_string(),
        }
    }

    #[test]
    fn personal_account_queries_v2_before_the_dead_v3() {
        assert_eq!(
            vec![(SETTING_V2, false), (SETTING_V3, false)],
            setting_routes(&credential(""))
        );
    }

    #[test]
    fn team_account_starts_with_the_organization_path() {
        let routes = setting_routes(&credential("org-123"));
        assert_eq!(SETTING_ORG, routes[0].0);
        assert!(routes[0].1, "组织路径必须带组织头");
        assert_eq!(SETTING_V2, routes[1].0);
        assert!(!routes[1].1, "退到个人路径要摘掉组织头");
    }

    #[test]
    fn the_dead_legacy_endpoint_is_not_in_the_setting_routes() {
        // 旧 points/v1/balance 只作最后兜底，不能排在 setting_info 前面。
        assert!(setting_routes(&credential(""))
            .iter()
            .all(|(path, _)| *path != "/api/web/points/v1/balance"));
    }
}
