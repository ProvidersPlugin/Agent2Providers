//! 钱包/冻结/今日调用是 snapshot，魔粒发放逐笔是 record。未知读数保留 null。
//! 不以余额差猜签到收益，也不把历史发放记录当成可用积分包。

use agent2api_wasm_sdk::{json, Result, Value};

use super::{
    client::{self, Client},
    clock, transactions, Region,
};

pub const BALANCE_PATH: &str = "/openapi/v1/magicubes/balance";
const RATE_LIMIT_PATH: &str = "/api/v1/inference/rate-limit";

pub fn current_balance(api: &Client) -> Result<Option<f64>> {
    let payload = api.site_json(BALANCE_PATH, "魔粒余额查询")?;
    Ok(transactions::number(
        &client::data_of(&payload),
        "available_balance",
    ))
}

pub fn query(region: Region) -> Result<Value> {
    let api = Client::for_account(region)?;
    let day = clock::today()?;
    let payload = api.site_json(BALANCE_PATH, "魔粒余额查询")?;
    let data = client::data_of(&payload);
    let available = transactions::number(&data, "available_balance");
    let frozen = transactions::number(&data, "frozen_amount");
    let mut credits = vec![
        json!({ "key": "available", "name": "可用余额", "balance": available, "unit": "魔粒", "snapshot": true }),
        json!({ "key": "frozen", "name": "冻结余额", "balance": frozen, "unit": "魔粒", "snapshot": true }),
    ];
    let mut warnings = Vec::new();
    let mut earned = None;
    let ledger_raw = match transactions::read(&api, &day) {
        Ok(ledger) => {
            if ledger.complete {
                earned = ledger.total_on(&day, false);
                credits.push(
                    json!({ "key": "earned-today", "name": "今日发放", "balance": earned,
                    "unit": "魔粒", "snapshot": true }),
                );
            } else {
                warnings.push("魔粒发放记录超过查询上限，今日合计未确认".to_string());
            }
            credits.extend(ledger.grants.iter().map(|grant| grant.credit_row()));
            Value::Array(ledger.raw)
        }
        Err(error) if matches!(error.code.as_deref(), Some("account_changed" | "forbidden")) => {
            return Err(error)
        }
        Err(error) => {
            warnings.push(error.message);
            Value::Null
        }
    };
    let calls_raw = match api.site_json(RATE_LIMIT_PATH, "今日用量查询") {
        Ok(payload) => {
            let used = transactions::number(&client::data_of(&payload), "currentUsagePerDay");
            credits.push(
                json!({ "key": "used-today", "name": "今日已用", "used": used,
                "unit": "次", "daily": true, "snapshot": true }),
            );
            payload
        }
        Err(error) if matches!(error.code.as_deref(), Some("account_changed" | "forbidden")) => {
            return Err(error)
        }
        Err(error) => {
            warnings.push(error.message);
            Value::Null
        }
    };
    let quota_sources: Vec<Value> = available.into_iter().map(|remaining| json!({
        "id": "account", "label": "账号主额度", "provider": region.provider_id(),
        "unit": "魔粒", "remaining": remaining, "total": null, "used": null, "unlimited": false
    })).collect();
    api.check()?;
    Ok(json!({
        "available": available, "unit": "魔粒", "earnedToday": earned,
        "credits": credits, "quotaSources": quota_sources,
        "wallets": [{ "type": "magicubes", "displayName": "魔粒余额", "balance": available,
            "frozen": frozen, "unit": "魔粒", "snapshot": true }],
        "warnings": warnings,
        "raw": { "balance": payload, "transactions": ledger_raw, "rateLimit": calls_raw }
    }))
}
