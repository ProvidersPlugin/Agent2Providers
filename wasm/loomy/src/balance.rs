//! V2 双钱包只读摘要，缺字段是未知；每日报告用厂商真实 remaining/limit，不编上限。
use super::{account, client, errors, ID};
use agent2api_wasm_sdk::{json, Result, Value};

const PATH: &str = "/api/v2/points/records?pageNo=1&pageSize=1&direction=debit&granularity=chat";

pub fn number(value: &Value, key: &str) -> Option<f64> {
    value[key]
        .as_f64()
        .or_else(|| value[key].as_str()?.trim().parse().ok())
        .filter(|number| number.is_finite())
}

pub fn read(record: &Value) -> Result<Value> {
    let session = account::token(&record["secret"])?;
    let payload = client::token_request("GET", PATH, &session, None, "积分查询")?;
    errors::ensure_success(&payload, "积分查询", &session)?;
    account::ensure_current(record)?;
    Ok(payload)
}

pub fn data(payload: &Value) -> &Value {
    payload.pointer("/data/data").unwrap_or(&payload["data"])
}

pub fn daily(payload: &Value) -> Option<f64> {
    let data = data(payload);
    number(data, "dailyRemainingPoints").or_else(|| number(data, "dailyBalance"))
}

pub fn query() -> Result<Value> {
    let record = account::current()?;
    let payload = read(&record)?;
    let data = data(&payload);
    let permanent = number(data, "balance");
    let daily = daily(&payload);
    let available = number(data, "availableBalance")
        .or_else(|| {
            permanent
                .zip(daily)
                .map(|(permanent, daily)| permanent + daily)
        })
        .filter(|number| number.is_finite());
    let daily_limit = number(data, "dailyLimitPoints").filter(|number| *number >= 0.0);
    let daily_used = daily_limit
        .zip(daily)
        .map(|(limit, daily)| (limit - daily).max(0.0));
    let reset = (agent2api_wasm_sdk::now() + 8 * 3_600_000).div_euclid(86_400_000) * 86_400_000
        + 86_400_000
        - 8 * 3_600_000;
    let wallets = vec![
        json!({ "type": "permanent_points", "displayName": "永久积分", "balance": permanent }),
        json!({ "type": "daily_points", "displayName": "每日赠送积分", "balance": daily,
            "remaining": daily, "limit": daily_limit, "used": daily_used, "resetAt": reset }),
    ];
    let mut sources = Vec::new();
    if let Some(available) = available {
        sources.push(json!({ "id": "account", "label": "账号主额度", "provider": ID,
            "unit": "积分", "remaining": available, "total": null, "used": null, "unlimited": false }));
    }
    if let Some(permanent) = permanent {
        sources.push(
            json!({ "id": "wallet:permanent_points", "label": "永久积分", "provider": ID,
            "unit": "积分", "remaining": permanent, "total": null, "used": null }),
        );
    }
    if let Some(daily) = daily {
        sources.push(json!({ "id": "wallet:daily_points", "label": "每日赠送积分", "provider": ID,
            "unit": "积分", "remaining": daily, "total": daily_limit, "used": daily_used, "resetAt": reset }));
    }
    Ok(
        json!({ "available": available, "unit": "积分", "wallets": wallets,
        "credits": [{ "name": "永久积分", "remain": permanent },
            { "name": "每日赠送积分", "remain": daily, "daily": true }], "quotaSources": sources }),
    )
}
