//! 签到与手动活动共用账号锁和持久在途台账；取消/超时后的未知结果只回查，不重复领取。
//! 只认当前国内账号清单中的免费 Credits 权益。每日与非每日调用者分别选目标，不互相代领。
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};
use super::{account, campaigns::{self, Session}, util};

const KIND: &str = "qoderCampaignClaims";
pub fn ledger(session: &Session) -> Result<Value> {
    let value = sdk::host("ledger.get", json!({ "kind": KIND }))?;
    if !value.is_null() && !value.is_object() { return Err(Error::http(409, "Qoder 活动台账格式无效，未提交领取")); }
    let identity = session.snapshot.credentials.binding();
    let mut value = if value["identity"] == identity { value } else { json!({ "identity": identity, "claims": {} }) };
    if !value["claims"].is_object() { value["claims"] = json!({}); }
    if let Some(claims) = value["claims"].as_object_mut() {
        let now = util::now()?;
        claims.retain(|_, row| row["at"].as_i64().is_some_and(|at| now.saturating_sub(at) < 90 * 86_400_000));
    }
    Ok(value)
}
fn save(session: &Session, value: &Value) -> Result<()> {
    account::unchanged(&session.snapshot)?;
    if value.to_string().len() > 60 * 1024 { return Err(Error::http(409, "Qoder 活动领取台账容量已满")); }
    sdk::host("ledger.put", json!({ "kind": KIND, "value": value })).map(|_| ())
}
pub fn apply(mut row: Value, saved: &Value) -> Value {
    if row["startsAt"] == saved["startsAt"] && row["endsAt"] == saved["endsAt"] {
        if row["status"] == "claimable" && matches!(saved["status"].as_str(), Some("claimed" | "confirming")) { row["status"] = saved["status"].clone(); }
        for key in ["claimedAt", "expiresAt"] { if !saved[key].is_null() { row[key] = saved[key].clone(); } }
    }
    row
}
pub fn result(id: &str, outcome: &str, campaign: Option<Value>) -> Value {
    json!({ "campaignId": id, "outcome": outcome, "campaign": campaign })
}
pub fn claim(session: Session, id: &str, daily: bool) -> Result<Value> {
    let id = id.trim();
    if !campaigns::id_valid(id) { return Err(Error::http(400, "请选择 Qoder 活动")); }
    let original = session.snapshot.clone();
    match claim_once(session, id, daily) {
        Err(error) if error.status == Some(401) && original.credentials.can_refresh() => {
            // 锁已随第一次返回释放；只在明确 401 时续期，重读活动再决定是否重试。
            account::unchanged(&original)?;
            let fresh = super::refresh::ensure(true)?;
            if fresh.credentials.binding() != original.credentials.binding() { return Err(account::changed()); }
            let session = campaigns::Session { list: campaigns::fetch(&fresh)?, snapshot: fresh };
            claim_once(session, id, daily)
        }
        result => result,
    }
}
fn claim_once(session: Session, id: &str, daily: bool) -> Result<Value> {
    if !campaigns::id_valid(id) { return Err(Error::http(400, "请选择 Qoder 活动")); }
    let _lock = AccountLock::acquire()?;
    account::unchanged(&session.snapshot)?;
    // 锁内重新读清单，避免另一轮已完成后仍使用旧 CLAIMABLE 快照。
    let list = campaigns::fetch(&session.snapshot)?;
    let target = list["campaigns"].as_array().into_iter().flatten()
        .find(|row| row["campaignId"] == id && campaigns::credit(row) && campaigns::daily(row) == daily);
    let Some(target) = target else { return Ok(result(id, "inactive", None)); };
    let now = util::now()?;
    let mut saved = ledger(&session)?;
    let campaign = apply(campaigns::view(target, now), &saved["claims"][id]);
    let status = campaign["status"].as_str().unwrap_or("ineligible").to_string();
    if status != "claimable" {
        return Ok(result(id, if status == "claimed" { "already_claimed" } else { &status }, Some(campaign)));
    }
    if saved["claims"].as_object().is_some_and(|claims| claims.len() >= 128 && !claims.contains_key(id)) {
        return Err(Error::http(409, "Qoder 活动记录过多，请稍后重试"));
    }
    saved["claims"][id] = json!({ "at": now, "status": "confirming", "startsAt": campaign["startsAt"], "endsAt": campaign["endsAt"] });
    save(&session, &saved)?;
    let response = campaigns::submit(&session.snapshot, id);
    let mut rejected = false;
    let mut failure = None;
    if let Ok(reply) = &response {
        if reply.status == 401 {
            if let Some(claims) = saved["claims"].as_object_mut() { claims.remove(id); }
            save(&session, &saved)?;
            return Err(super::errors::failure(401, &reply.value));
        }
        let receipt = campaigns::unwrap_data(reply.value.clone());
        if ((200..300).contains(&reply.status) || reply.status == 409) && receipt["result"] == "ALREADY_CLAIMED" {
            return finish(&session, id, saved, "already_claimed", receipt_view(campaign, &receipt));
        }
        if (200..300).contains(&reply.status) && !super::errors::has_error(&reply.value) && receipt["status"] == "CLAIMED" {
            let outcome = if receipt["replayed"] == true { "already_claimed" } else { "claimed" };
            return finish(&session, id, saved, outcome, receipt_view(campaign, &receipt));
        }
        rejected = (400..500).contains(&reply.status) && reply.status != 408;
        if !(200..300).contains(&reply.status) || super::errors::has_error(&reply.value) {
            failure = Some(super::errors::failure(reply.status, &reply.value));
        }
    } else if let Err(error) = response { failure = Some(error); }
    if let Some(error) = failure.as_ref().filter(|error| util::terminal(error)) { return Err(error.clone()); }
    // 只有回查真实 CLAIMED 才确认；HTTP 200、空响应或 409 本身不是领取证据。
    match campaigns::fetch(&session.snapshot) {
        Ok(list) => {
            if let Some(row) = list["campaigns"].as_array().into_iter().flatten().find(|row| row["campaignId"] == id) {
                if campaigns::status(row, util::now()?) == "claimed" {
                    return finish(&session, id, saved, "already_claimed", campaigns::view(row, util::now()?));
                }
            }
        }
        Err(error) if util::terminal(&error) => return Err(error),
        Err(_) => {}
    }
    let outcome = if rejected { "failed" } else { "confirming" };
    let result = finish(&session, id, saved, outcome, campaign)?;
    if rejected { if let Some(error) = failure { return Err(error); } }
    Ok(result)
}
fn receipt_view(mut view: Value, value: &Value) -> Value {
    for key in ["claimedAt", "expiresAt"] { if let Some(at) = util::timestamp(&value[key]) { view[key] = json!(at); } }
    if let Some(amount) = util::number(&value["benefit"]["amount"]).filter(|amount| *amount >= 0.0) { view["amount"] = json!(amount); }
    view
}
fn finish(session: &Session, id: &str, mut saved: Value, outcome: &str, mut campaign: Value) -> Result<Value> {
    let status = match outcome { "claimed" | "already_claimed" => "claimed", "confirming" => "confirming", _ => "ineligible" };
    campaign["status"] = json!(status);
    if matches!(status, "claimed" | "confirming") {
        saved["claims"][id] = json!({ "at": util::now()?, "status": status,
            "startsAt": campaign["startsAt"], "endsAt": campaign["endsAt"], "claimedAt": campaign["claimedAt"], "expiresAt": campaign["expiresAt"] });
    } else if let Some(claims) = saved["claims"].as_object_mut() { claims.remove(id); }
    save(session, &saved)?;
    Ok(result(id, outcome, Some(campaign)))
}
