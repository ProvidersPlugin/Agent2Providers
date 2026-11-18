//! 非每日免费 Credits 活动预览与点名领取；不把 VIEW_DETAILS 或付费促销当福利。
use agent2api_wasm_sdk::{json, Error, Result, Value};
use super::{campaigns, claims, util};

pub fn preview() -> Result<Value> {
    let session = campaigns::load()?;
    let saved = claims::ledger(&session)?;
    let now = util::now()?;
    let rows = session.list["campaigns"].as_array().into_iter().flatten()
        .filter(|row| campaigns::credit(row) && !campaigns::daily(row)).map(|row| {
            let id = row["campaignId"].as_str().unwrap_or("");
            claims::apply(campaigns::view(row, now), &saved["claims"][id])
        }).collect::<Vec<_>>();
    Ok(json!({ "campaigns": rows, "riskNote": null }))
}
pub fn claim(id: &str) -> Result<Value> {
    if !campaigns::id_valid(id.trim()) { return Err(Error::http(400, "请选择 Qoder 活动")); }
    let session = campaigns::load()?;
    claims::claim(session, id, false)
}
