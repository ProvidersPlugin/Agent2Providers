//! 国际账务使用现有 usage/windows/credits/admin 契约，不增加不可到达的 UI。
//! 主余额 available=null：原始资金数值单位待确认，不能参与积分/货币合计或限额判断。
//! wallet 主请求失败报错；附加流水/订阅失败分别 unknown + warning，不补空数据。

use crate::{
    budget::Budget,
    client,
    endpoints::ReadEndpoint,
    errors,
    snapshot::{self, Snapshot},
    subscription, transactions, util,
};
use agent2api_wasm_sdk::{json, Result, Value};

fn read(account: &Snapshot, budget: &Budget) -> Result<Value> {
    account.unchanged()?;
    let result = client::get(&account.session, ReadEndpoint::Wallet, &[], budget);
    account.unchanged()?;
    let data = result?.data;
    let mut guard = account.guard.clone();
    guard.observe(&data)?;
    let balance = util::number(&data, "balance");
    let remaining = util::nonnegative(&data, "daily_token_balance");
    let limit = util::nonnegative(&data, "daily_token_limit");
    let mut warnings = vec![
        "官方钱包协议有据；国际资金单位及换算未验证，保留原始值，不换算为积分、美元或美分"
            .to_string(),
    ];
    if balance.is_none() {
        warnings.push("钱包未返回可识别余额，保持未知而非零".into());
    }
    if remaining.is_none() || limit.is_none() {
        warnings.push("每日 Token 原始读数不完整，未补零或从资金余额推算".into());
    }
    warnings.push(
        "daily_token_* 按官方字段作为 Token 读数单列，未做国际验收；重置时区及额度池关系未知"
            .into(),
    );
    Ok(
        json!({ "id": guard.field(&data, "id", 128), "balanceRaw": balance,
        "balance": null, "unit": "单位待确认", "unitKnown": false, "converted": false,
        "dailyTokens": { "remaining": remaining, "limit": limit, "unit": "tokens", "internationalVerified": false,
            "resetAt": null, "resetTimezone": null },
        "readOnly": true, "internationalVerified": false, "warnings": warnings }),
    )
}

fn raw_view(value: &Value) -> String {
    match value.as_f64() {
        Some(value) => format!("原始值 {value}；单位待确认"),
        None => "原始值未知；单位待确认".into(),
    }
}

fn token_view(remaining: &Value, limit: &Value) -> String {
    let remaining = remaining
        .as_f64()
        .map(|value| value.to_string())
        .unwrap_or_else(|| "未知".into());
    let limit = limit
        .as_f64()
        .map(|value| value.to_string())
        .unwrap_or_else(|| "未知".into());
    format!("剩余 {remaining} / 上限 {limit} tokens（国际未验收）")
}

pub fn query() -> Result<Value> {
    let account = snapshot::current()?;
    let budget = Budget::new()?;
    let wallet = read(&account, &budget)?;
    let mut warnings = wallet["warnings"].as_array().cloned().unwrap_or_default();
    let mut failures = serde_json::Map::new();
    let ledger = match transactions::read(&account, &json!({}), &budget) {
        Ok(value) => {
            warnings.extend(value["warnings"].as_array().into_iter().flatten().cloned());
            Some(value)
        }
        Err(error) => {
            account.unchanged()?;
            // 宿主取消/撤销权限仍停止整次操作；预算用尽只影响剩余附加读数。
            if errors::host_stop(&error)
                && !matches!(
                    error.code.as_deref(),
                    Some(
                        "monkeycode_intl_deadline_exceeded"
                            | "timeout"
                            | "deadline_exceeded"
                            | "plugin_timeout"
                    )
                )
            {
                return Err(error);
            }
            warnings.push(json!(format!("流水未知：{}", error.message)));
            failures.insert("transactions".into(), errors::report(&error));
            None
        }
    };
    let subscription = match subscription::read(&account, &budget) {
        Ok(value) => {
            warnings.extend(value["warnings"].as_array().into_iter().flatten().cloned());
            Some(value)
        }
        Err(error) => {
            account.unchanged()?;
            if errors::host_stop(&error)
                && !matches!(
                    error.code.as_deref(),
                    Some(
                        "monkeycode_intl_deadline_exceeded"
                            | "timeout"
                            | "deadline_exceeded"
                            | "plugin_timeout"
                    )
                )
            {
                return Err(error);
            }
            warnings.push(json!(format!("订阅未知：{}", error.message)));
            failures.insert("subscription".into(), errors::report(&error));
            None
        }
    };
    let raw = &wallet["balanceRaw"];
    let daily = &wallet["dailyTokens"];
    let mut credits = vec![
        json!({ "key": "wallet-raw", "name": "钱包原始余额（单位待确认）", "balance": raw, "unit": "单位待确认", "snapshot": true }),
        json!({ "key": "daily-token-balance", "name": "每日 Token 剩余（国际未验收）", "balance": daily["remaining"], "unit": "tokens", "snapshot": true }),
        json!({ "key": "daily-token-limit", "name": "每日 Token 上限（国际未验收）", "balance": daily["limit"], "unit": "tokens", "snapshot": true }),
    ];
    if let Some(ledger) = &ledger {
        credits.extend(
            ledger["transactions"]
                .as_array()
                .into_iter()
                .flatten()
                .cloned(),
        );
    }
    account.unchanged()?;
    Ok(json!({
        "available": null, "balance": null, "unit": "单位待确认", "unitKnown": false,
        "balanceView": raw_view(raw), "wallet": wallet, "dailyTokens": daily,
        "windows": [
            { "key": "wallet-raw", "label": "钱包原始余额", "window": "单位待确认", "view": raw_view(raw), "aside": true },
            { "key": "daily-tokens", "label": "每日 Token（国际未验收）", "window": "每日 Token（国际未验收）",
              "view": token_view(&daily["remaining"], &daily["limit"]), "aside": true }
        ],
        "wallets": [
            { "type": "wallet-raw", "displayName": "钱包原始余额（单位待确认）", "balance": raw,
              "unit": "单位待确认", "balanceView": raw_view(raw), "snapshot": true },
            { "type": "daily-tokens", "displayName": "每日 Token（国际未验收）", "balance": daily["remaining"],
              "limit": daily["limit"], "unit": "tokens", "balanceView": token_view(&daily["remaining"], &daily["limit"]), "snapshot": true }
        ],
        "credits": credits, "transactions": ledger.as_ref().map(|ledger| &ledger["transactions"]),
        "transactionPage": ledger.as_ref().map(|ledger| &ledger["page"]), "subscription": subscription,
        "transactionState": if ledger.is_some() { "observed_unverified" } else { "unknown" },
        "subscriptionState": subscription.as_ref().and_then(|value| value["state"].as_str()).unwrap_or("unknown"),
        "warnings": warnings, "errors": failures, "quotaSources": [],
        "readOnly": true, "internationalVerified": false, "fetchedAt": util::now()?
    }))
}

pub fn wallet_query() -> Result<Value> {
    read(&snapshot::current()?, &Budget::new()?)
}

pub fn transactions_query(params: &Value) -> Result<Value> {
    transactions::read(&snapshot::current()?, params, &Budget::new()?)
}

pub fn subscription_query() -> Result<Value> {
    subscription::read(&snapshot::current()?, &Budget::new()?)
}

pub fn sources() -> Value {
    // 未验收的国际读数不喂给自动限额/账户轮换；仅在可见窗口和原始数值展示。
    json!([])
}
