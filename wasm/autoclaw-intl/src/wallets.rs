//! v2 钱包优先，明确业务失败或缺 wallets 才降级 v1；未知数值留空，不按零合计。
use agent2api_wasm_sdk::{json, Result, Value};
use super::{client, util};

pub fn query(token: &str) -> Result<Value> {
    let primary = client::get("/agent-assetmgr/api/v2/wallets?biz_app_id=autoclaw", token, "积分查询")?;
    if util::integer(&primary["code"]).is_some_and(client::auth_code) { client::business(&primary, "查询积分")?; }
    let (total, wallets) = if util::integer(&primary["code"]) == Some(0) && primary["data"]["wallets"].is_array() {
        (util::number(&primary["data"]["total_balance"]), v2(&primary["data"]["wallets"]))
    } else {
        let legacy = client::get("/agent-assetmgr/api/v1/wallet-instances?wallet_type=all&wallet_scope=all", token, "旧版积分查询")?;
        client::strict_business(&legacy, "查询积分")?;
        if !legacy["data"].is_object() || !legacy["data"]["wallet_instances"].is_array() {
            return Err(agent2api_wasm_sdk::Error::http(502, "AutoClaw 旧版积分钱包缺少有效数据"));
        }
        (util::number(&legacy["data"]["total_balance"]), v1(&legacy["data"]["wallet_instances"]))
    };
    let expiring = match client::get("/agent-assetmgr/api/v1/points/expiring?biz_app_id=autoclaw", token, "积分到期查询") {
        Ok(payload) if util::integer(&payload["code"]) == Some(0) && payload["data"].is_object() => {
            let data = &payload["data"];
            json!({ "points": util::number(&data["expiring_points"]), "text": util::text(data, &["expiring_points_text"]),
                "totalPoints": util::number(&data["total_points"]), "expireTimeValue": data["expire_time_value"], "expireTimeType": data["expire_time_type"] })
        }
        Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
        _ => Value::Null,
    };
    Ok(json!({ "totalBalance": total, "wallets": wallets, "expiring": expiring }))
}
fn v2(value: &Value) -> Vec<Value> {
    let mut rows = value.as_array().into_iter().flatten().filter(|wallet| util::truthy(&wallet["display"]))
        .map(|wallet| {
            let name = util::text(wallet, &["display_name", "public_wallet_type"]);
            let balance = util::number(&wallet["balance"]);
            let display = util::text(wallet, &["balance_view"]);
            (util::integer(&wallet["priority"]).unwrap_or(i64::MAX), json!({ "type": util::text(wallet, &["public_wallet_type"]),
                "displayName": if name.is_empty() { "积分" } else { &name }, "balance": balance,
                "balanceView": if display.is_empty() { balance.map(|value| value.to_string()).unwrap_or_default() } else { display } }))
        }).collect::<Vec<_>>();
    rows.sort_by_key(|(priority, _)| *priority);
    rows.into_iter().map(|(_, value)| value).collect()
}
fn v1(value: &Value) -> Vec<Value> {
    let mut rows: Vec<(String, Option<f64>)> = Vec::new();
    for wallet in value.as_array().into_iter().flatten() {
        if !util::text(wallet, &["status"]).eq_ignore_ascii_case("active") || !util::truthy(&wallet["display"]) { continue; }
        let scope = util::text(wallet, &["wallet_scope"]).to_ascii_lowercase().replace("wallet_scope_", "");
        let scope = if scope.is_empty() { "other".to_string() } else { scope };
        let balance = util::number(&wallet["balance"]);
        if let Some((_, total)) = rows.iter_mut().find(|(kind, _)| *kind == scope) {
            *total = (*total).zip(balance).map(|(total, balance)| total + balance).filter(|total| total.is_finite());
        } else { rows.push((scope, balance)); }
    }
    rows.into_iter().map(|(kind, balance)| {
        let name = match kind.as_str() { "daily" => "赠送积分", "monthly" => "订阅积分", "long_term" | "annual" => "通用积分", "campaign" => "活动积分", _ => "其他积分" };
        json!({ "type": kind, "displayName": name, "balance": balance, "balanceView": balance.map(|value| value.to_string()).unwrap_or_default() })
    }).collect()
}

pub fn credits(points: &Value) -> Vec<Value> {
    let mut rows = points["wallets"].as_array().into_iter().flatten().map(|wallet| {
        let name = util::text(wallet, &["displayName", "type"]);
        let mut row = json!({ "name": if name.is_empty() { "积分" } else { &name } });
        if let Some(value) = util::number(&wallet["balance"]) { row["remain"] = json!(value); }
        row
    }).collect::<Vec<_>>();
    if let Some(value) = util::number(&points["expiring"]["points"]).filter(|value| *value > 0.0) {
        let mut row = json!({ "name": "即将过期", "remain": value });
        let text = util::text(&points["expiring"], &["text"]);
        if !text.is_empty() { row["grantReason"] = json!(text); }
        rows.push(row);
    }
    rows
}
