//! 非每日免费 Credits 活动：预览并点名领取，不把 VIEW_DETAILS 或付费促销转成福利。
//! 普通清单可读，风控未接通不伪称活动不存在；服务端未下发的活动不能手填编号领取。
use super::{account, campaigns, claims, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn preview() -> Result<Value> {
    let session = campaigns::load()?;
    let saved = claims::ledger(&session)?;
    let now = util::now()?;
    let rows = session.list["campaigns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| campaigns::credit(row) && !campaigns::daily(row))
        .map(|row| {
            let id = row["campaignId"].as_str().unwrap_or("");
            claims::apply(campaigns::view(row, now), &saved["claims"][id])
        })
        .collect::<Vec<_>>();
    account::unchanged(&session.snapshot)?;
    Ok(
        json!({ "campaigns": rows, "riskNote": session.risk_note, "riskReady": session.risk_ready,
        "catalogComplete": session.risk_ready }),
    )
}
pub fn claim(id: &str) -> Result<Value> {
    if !campaigns::id_valid(id.trim()) {
        return Err(Error::http(400, "请选择 Qoder 活动"));
    }
    let session = campaigns::load()?;
    let risk_note = session.risk_note.clone();
    let mut result = claims::claim(session, id, false)?;
    result["riskNote"] = json!(risk_note);
    Ok(result)
}
