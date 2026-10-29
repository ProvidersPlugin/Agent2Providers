//! 魔粒钱包、今日发放和调用次数。钱包与计数器使用 snapshot，发放逐笔使用 record。

use serde_json::{json, Value};

use super::client::{self, ControlClient};
use super::transactions;

pub const BALANCE_PATH: &str = "/openapi/v1/magicubes/balance";
const RATE_LIMIT_PATH: &str = "/api/v1/inference/rate-limit";

pub async fn current_balance(api: &ControlClient) -> Result<Option<f64>, String> {
    let payload = api.site_json(BALANCE_PATH, "魔粒余额查询").await?;
    Ok(transactions::number(&client::data_of(&payload), "available_balance"))
}

pub async fn query_usage(api: &ControlClient) -> Result<Value, String> {
    let day = crate::beijing::today_key();
    let (wallet, ledger, calls) = tokio::join!(
        api.site_json(BALANCE_PATH, "魔粒余额查询"),
        transactions::read(api, &day),
        api.site_json(RATE_LIMIT_PATH, "今日用量查询"),
    );
    let payload = wallet?;
    let data = client::data_of(&payload);
    let available = transactions::number(&data, "available_balance");
    let frozen = transactions::number(&data, "frozen_amount");
    let mut credits = vec![
        json!({ "key": "available", "name": "可用余额", "balance": available, "unit": "魔粒", "snapshot": true }),
        json!({ "key": "frozen", "name": "冻结余额", "balance": frozen, "unit": "魔粒", "snapshot": true }),
    ];
    let mut warnings = Vec::new();
    let mut earned = None;
    let ledger_raw = match ledger {
        Ok(ledger) => {
            if ledger.complete {
                earned = ledger.total_on(&day, false);
                credits.push(json!({
                    "key": "earned-today", "name": "今日发放", "balance": earned,
                    "unit": "魔粒", "snapshot": true,
                }));
            } else {
                warnings.push("魔粒发放记录超过查询上限，今日合计未确认".to_string());
            }
            credits.extend(ledger.grants.iter().map(|grant| grant.credit_row()));
            Value::Array(ledger.raw)
        }
        Err(error) => { warnings.push(error); Value::Null }
    };
    let calls_raw = match calls {
        Ok(payload) => {
            let used = transactions::number(&client::data_of(&payload), "currentUsagePerDay");
            credits.push(json!({
                "key": "used-today", "name": "今日已用", "used": used,
                "unit": "次", "daily": true, "snapshot": true,
            }));
            payload
        }
        Err(error) => { warnings.push(error); Value::Null }
    };
    Ok(json!({
        "available": available,
        "unit": "魔粒",
        "earnedToday": earned,
        "credits": credits,
        "wallets": [{
            "type": "magicubes", "displayName": "魔粒余额", "balance": available,
            "frozen": frozen, "unit": "魔粒", "snapshot": true,
        }],
        "warnings": warnings,
        "raw": { "balance": payload, "transactions": ledger_raw, "rateLimit": calls_raw },
    }))
}
