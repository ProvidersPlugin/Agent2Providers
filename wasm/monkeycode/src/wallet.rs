//! 国内官方钱包字段：balance / 1000 是积分，daily_token_* 独立计数。
//! GET 失败直接返回真实错误；流水或订阅局部失败单独报告，不捏造空账单或基础套餐。

use agent2api_wasm_sdk::{json, Result, Value};

use crate::{
    account, client,
    endpoints::{self, ReadEndpoint},
    errors, subscription, transactions, util,
};

fn wallet(account: &account::Snapshot) -> Result<Value> {
    account.unchanged()?;
    let result = client::get(&account.session, ReadEndpoint::Wallet, &[]);
    account.unchanged()?;
    let data = result?;
    let raw = util::number(&data, "balance");
    let remaining = util::number(&data, "daily_token_balance").filter(|value| *value >= 0.0);
    let limit = util::number(&data, "daily_token_limit").filter(|value| *value >= 0.0);
    Ok(json!({
        "id": util::public_text(util::text(&data, &["id"]), &account.session, 128),
        "balance": raw.map(|value| value / 1000.0), "unit": "积分",
        "balanceRaw": raw, "rawUnit": "毫积分",
        "dailyTokens": { "remaining": remaining, "limit": limit, "unit": "tokens", "daily": true },
        "readOnly": true
    }))
}

pub fn query() -> Result<Value> {
    let account = account::current()?;
    let wallet = wallet(&account)?;
    let mut warnings = Vec::new();
    let mut failures = serde_json::Map::new();
    let ledger = match transactions::read(&account, &json!({})) {
        Ok(value) => value,
        Err(error) if errors::host_stop(&error) => return Err(error),
        Err(error) => {
            warnings.push(error.message.clone());
            failures.insert("transactions".into(), errors::report(&error));
            Value::Null
        }
    };
    let subscription = match subscription::read(&account) {
        Ok(value) => value,
        Err(error) if errors::host_stop(&error) => return Err(error),
        Err(error) => {
            warnings.push(error.message.clone());
            failures.insert("subscription".into(), errors::report(&error));
            Value::Null
        }
    };
    account.unchanged()?;
    if wallet["balance"].is_null() {
        warnings.push("钱包没有返回可识别的积分余额，保留未知值".into());
    }
    if wallet["dailyTokens"]["remaining"].is_null() || wallet["dailyTokens"]["limit"].is_null() {
        warnings.push("每日免费 Token 读数不完整，未用积分余额或零值代替".into());
    }
    let mut credits = vec![
        json!({ "key": "available", "name": "可用余额", "balance": wallet["balance"], "unit": "积分", "snapshot": true }),
        json!({ "key": "daily-tokens", "name": "每日免费 Token 剩余", "balance": wallet["dailyTokens"]["remaining"],
            "unit": "tokens", "daily": true, "snapshot": true }),
        json!({ "key": "daily-token-limit", "name": "每日免费 Token 上限", "balance": wallet["dailyTokens"]["limit"],
            "unit": "tokens", "daily": true, "snapshot": true }),
    ];
    if let Some(rows) = ledger["transactions"].as_array() {
        credits.extend(rows.iter().cloned());
    }
    let mut report = json!({
        "available": wallet["balance"], "unit": "积分", "wallet": wallet,
        "dailyTokens": wallet["dailyTokens"],
        "wallets": [
            { "type": "points", "displayName": "积分余额", "balance": wallet["balance"], "unit": "积分", "snapshot": true },
            { "type": "daily-tokens", "displayName": "每日免费 Token", "balance": wallet["dailyTokens"]["remaining"],
                "limit": wallet["dailyTokens"]["limit"], "unit": "tokens", "snapshot": true }
        ],
        "credits": credits, "transactions": ledger["transactions"], "transactionPage": ledger["page"],
        "subscription": subscription, "warnings": warnings, "errors": failures,
        "readOnly": true, "fetchedAt": util::now()?
    });
    report["quotaSources"] = sources(&report);
    Ok(report)
}

pub fn sources(report: &Value) -> Value {
    let mut rows = Vec::new();
    if let Some(remaining) = util::number(report, "available") {
        rows.push(json!({ "id": "account", "label": "积分余额", "provider": endpoints::ID,
            "remaining": remaining, "total": null, "used": null, "unit": "积分", "unlimited": false }));
    }
    if let Some(remaining) = util::number(&report["dailyTokens"], "remaining") {
        rows.push(
            json!({ "id": "daily-tokens", "label": "每日免费 Token", "provider": endpoints::ID,
            "remaining": remaining, "total": report["dailyTokens"]["limit"], "used": null,
            "unit": "tokens", "unlimited": false }),
        );
    }
    json!(rows)
}

pub fn wallet_query() -> Result<Value> {
    wallet(&account::current()?)
}

pub fn transactions_query(params: &Value) -> Result<Value> {
    transactions::read(&account::current()?, params)
}

pub fn subscription_query() -> Result<Value> {
    subscription::read(&account::current()?)
}
