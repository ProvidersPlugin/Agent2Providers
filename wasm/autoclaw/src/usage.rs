//! 积分主结果不受附加订阅/邮箱查询失败影响；余额查询不隐式消耗刷新令牌。
use agent2api_wasm_sdk::{json, Error, Result, Value};
use super::{account, endpoints, profile, subscription, util, wallets};

pub fn query() -> Result<Value> {
    let snapshot = account::read()?;
    let points = wallets::query(&account::access(&snapshot.secret))?;
    let (subscription, subscription_error) = match subscription::query(&snapshot.secret) {
        Ok(value) => (value, Value::Null),
        Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
        Err(error) => (Value::Null, json!(error.message)),
    };
    let current = account::read()?;
    if !account::same(&snapshot, &current) { return Err(Error::http(409, "AutoClaw 积分查询期间账号已变化，请重试")); }
    if let Err(error) = profile::fill_email(&snapshot) {
        if error.code.as_deref() == Some("forbidden") { return Err(error); }
    }
    let credits = wallets::credits(&points);
    let mut report = json!({ "available": points["totalBalance"], "unit": "积分", "wallets": points["wallets"],
        "subscription": subscription, "raw": { "expiring": points["expiring"], "subscriptionError": subscription_error } });
    if !credits.is_empty() { report["credits"] = json!(credits); }
    report["quotaSources"] = sources(&report);
    Ok(report)
}
pub fn sources(report: &Value) -> Value {
    let mut sources = Vec::new();
    if let Some(available) = util::number(&report["available"]) {
        sources.push(json!({ "id": "account", "provider": endpoints::PROVIDER, "label": "账号主额度", "unit": "积分",
            "remaining": available, "total": null, "used": null, "unlimited": false }));
    }
    for wallet in report["wallets"].as_array().into_iter().flatten() {
        let kind = util::text(wallet, &["type"]);
        let Some(remaining) = util::number(&wallet["balance"]) else { continue; };
        if kind.is_empty() { continue; }
        let mut row = json!({ "id": format!("wallet:{kind}"), "provider": endpoints::PROVIDER,
            "label": util::text(wallet, &["displayName"]), "unit": "积分", "remaining": remaining,
            "total": util::number(&wallet["limit"]), "used": util::number(&wallet["used"]) });
        if wallet["window"].is_object() { row["window"] = wallet["window"].clone(); }
        sources.push(row);
    }
    json!(sources)
}
