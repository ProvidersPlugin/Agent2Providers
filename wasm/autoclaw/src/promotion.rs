//! 活动奖励是 user-info 登录活跃后自动入账，没有独立领取接口。
//! 按既有证据以北京 grant_date + event_type/activity_id 识别所有当日活动入账，
//! 不固定承诺 20000/500 等历史金额；以实际流水合计，查询失败不假装无活动。
use agent2api_wasm_sdk::{json, Error, Result, Value};
use super::{account, client, util};

const PATH: &str = "/agent-assetmgr/api/v1/ledgers_std?asset_type=point&wallet_type=all&cursor=&page_size=50&flow_direction=income";

pub fn today(secret: &Value) -> Result<Value> {
    let payload = client::get(PATH, &account::access(secret), "活动奖励查询")?;
    client::business(&payload, "查询活动奖励")?;
    let today = util::day()?;
    let entries = payload["data"]["entries"].as_array().ok_or_else(|| Error::http(502, "AutoClaw 活动流水没有有效 entries 数组"))?;
    let found = entries.iter().filter(|entry| is_today(entry, &today)).collect::<Vec<_>>();
    if found.is_empty() { return Ok(json!({ "granted": false, "grantDate": today })); }
    let mut total = 0_i64;
    let mut known = true;
    let detail = found.iter().map(|entry| {
        let points = util::integer(&entry["amount"]);
        if let Some(sum) = points.and_then(|points| total.checked_add(points)) { total = sum; } else { known = false; }
        json!({ "points": points, "description": util::text(entry, &["description"]),
            "activityId": util::text(&entry["metadata"], &["activity_id"]) })
    }).collect::<Vec<_>>();
    Ok(json!({ "granted": true, "points": if known { Some(total) } else { None }, "grantDate": today,
        "description": found.first().map(|entry| util::text(entry, &["description"])).unwrap_or_default(), "entries": detail }))
}
fn is_today(entry: &Value, today: &str) -> bool {
    let metadata = &entry["metadata"];
    if metadata["grant_date"] != today { return false; }
    let event = util::text(metadata, &["event_type"]);
    if ["autoclaw_user_active", "autoclaw_user_signup_day"].iter().any(|prefix| event.starts_with(*prefix)) { return true; }
    let activity = util::text(metadata, &["activity_id"]);
    ["autoclaw_user_active_reward", "autoclaw_user_active_bonus", "autoclaw_new_user_day_1_to_7_bonus"].iter().any(|prefix| activity.starts_with(*prefix))
}
pub fn activate(secret: &Value) -> Result<()> {
    let payload = client::post("/userapi/v1/user-info", &account::access(secret),
        &json!({ "source_id": "autoclaw", "device_id": account::device(secret) }), "登录活跃触发")?;
    client::strict_business(&payload, "触发登录活跃")
}
pub fn attach(mut claim: Value, reward: Value) -> Value {
    if reward["granted"] == true {
        if let Some(points) = util::integer(&reward["points"]).filter(|points| *points > 0) {
            let message = util::text(&claim, &["msg"]);
            if !message.is_empty() { claim["msg"] = json!(format!("{message}（活动福利 +{points} 已到账）")); }
        }
    }
    claim["promotionReward"] = reward;
    claim
}
