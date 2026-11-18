//! getgfvipremain真实积分，数值/字符串双读。余额不是单次对话token用量。
//! 本地原生只有totalPoint、bonusPoint证据；无月总额/已用证据时不合成月额度。

use crate::{endpoints, errors, network, session, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn sources(report: &Value) -> Value {
    match util::number(report.get("available")) {
        Some(remaining) => {
            json!([{ "provider": "kuku", "id": "account", "label": "账号主额度", "unit": "积分",
            "remaining": remaining, "total": null, "used": null }])
        }
        None => json!([]),
    }
}
pub fn query() -> Result<Value> {
    let (_, c) = session::read()?;
    let value = network::get(
        &format!(
            "{}{}?{}",
            endpoints::BASE,
            endpoints::BALANCE,
            endpoints::BALANCE_QUERY
        ),
        network::headers(&c),
        &c.secrets(),
    )?;
    errors::status(&value, &c.secrets())?;
    session::unchanged(&c)?;
    let rows = value["data"]["list"]
        .as_array()
        .ok_or_else(|| Error::http(502, "库库积分响应缺少资产清单"))?;
    let selected = rows
        .iter()
        .find(|row| row["assetName"] == "token")
        .or_else(|| {
            rows.first()
                .filter(|row| row["assetName"].as_str().is_none_or(str::is_empty))
        });
    let amount = selected.and_then(|row| util::number(row.get("totalPoint")));
    let bonus = selected.and_then(|row| util::number(row.get("bonusPoint")));
    let raw_rows: Vec<_> = rows
        .iter()
        .take(128)
        .map(|row| {
            json!({ "assetType": row["assetType"],
        "assetName": row["assetName"], "totalPoint": util::number(row.get("totalPoint")),
        "bonusPoint": util::number(row.get("bonusPoint")) })
        })
        .collect();
    let mut report = json!({ "available": amount, "unit": "积分", "wallets": [{ "type": "points", "displayName": "积分", "balance": amount }],
        "subscription": null, "raw": { "vipRemain": raw_rows, "bonusPoint": bonus } });
    if amount.is_none() {
        report["message"] = json!("库库未返回有效剩余积分，未知不显示为0");
    }
    report["quotaSources"] = sources(&report);
    Ok(errors::sanitize(&report, &c.secrets()))
}
