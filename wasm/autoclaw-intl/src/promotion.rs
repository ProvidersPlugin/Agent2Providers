//! 活动奖励是 user-info 登录活跃后自动入账，没有独立领取接口。
//! 按既有证据以北京 grant_date + event_type/activity_id 识别所有当日活动入账，
//! 不固定承诺 20000/500 等历史金额；以实际流水合计，查询失败不假装无活动。
use super::{account, client, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

const PATH: &str = "/agent-assetmgr/api/v1/ledgers_std?asset_type=point&wallet_type=all&cursor=&page_size=50&flow_direction=income";

pub fn inspect(secret: &Value) -> Result<Value> {
    let payload = client::get(PATH, &account::access(secret), "活动奖励查询")?;
    client::business(&payload, "查询活动奖励")?;
    let today = util::day()?;
    let entries = payload["data"]["entries"]
        .as_array()
        .ok_or_else(|| Error::http(502, "AutoClaw 活动流水没有有效 entries 数组"))?;
    let matched = entries
        .iter()
        .filter(|entry| is_reward(entry))
        .collect::<Vec<_>>();
    let latest = matched.first().map(|entry| summary(entry));
    let found = matched
        .into_iter()
        .filter(|entry| entry["metadata"]["grant_date"] == today)
        .collect::<Vec<_>>();
    let incomplete = found.is_empty()
        && entries.len() >= 50
        && entries
            .last()
            .and_then(|entry| entry["metadata"]["grant_date"].as_str())
            .is_none_or(|date| date >= today.as_str());
    if found.is_empty() {
        return Ok(json!({
            "granted": false,
            "grantDate": today,
            "incomplete": incomplete,
            "latest": latest
        }));
    }
    let mut total = 0_i64;
    let mut known = !incomplete && entries.len() < 50;
    let detail = found
        .iter()
        .map(|entry| {
            let points = util::integer(&entry["amount"]);
            if let Some(sum) = points.and_then(|points| total.checked_add(points)) {
                total = sum;
            } else {
                known = false;
            }
            summary(entry)
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "granted": true,
        "points": if known { Some(total) } else { None },
        "grantDate": today,
        "description": found.first().map(|entry| util::text(entry, &["description"])).unwrap_or_default(),
        "entries": detail,
        "incomplete": false,
        "latest": latest
    }))
}

pub fn today(secret: &Value) -> Result<Value> {
    let report = inspect(secret)?;
    if report["granted"] != true && report["incomplete"] == true {
        return Err(Error::http(
            502,
            "AutoClaw 当日活动收入窗口未完整，暂不能确认是否已到账",
        ));
    }
    Ok(report)
}

fn summary(entry: &Value) -> Value {
    json!({
        "points": util::integer(&entry["amount"]),
        "grantDate": util::text(&entry["metadata"], &["grant_date"]),
        "description": util::text(entry, &["description"]),
        "activityId": util::text(&entry["metadata"], &["activity_id"]),
        "eventType": util::text(&entry["metadata"], &["event_type"]),
    })
}

fn is_reward(entry: &Value) -> bool {
    let metadata = &entry["metadata"];
    let event = util::text(metadata, &["event_type"]);
    if ["autoclaw_user_active", "autoclaw_user_signup_day", "autoclaw_weekly_perks"]
        .iter()
        .any(|prefix| event.starts_with(*prefix))
        || event.contains("weekly_perks")
    {
        return true;
    }
    let activity = util::text(metadata, &["activity_id"]);
    [
        "autoclaw_user_active_reward",
        "autoclaw_user_active_bonus",
        "autoclaw_new_user_day_1_to_7_bonus",
        "autoclaw_weekly_perks",
    ]
    .iter()
    .any(|prefix| activity.starts_with(*prefix))
        || activity.contains("weekly_perks")
}
pub fn activate(secret: &Value) -> Result<()> {
    let payload = client::activate(
        &account::access(secret),
        &json!({ "source_id": "autoclaw", "device_id": account::device(secret) }),
    )?;
    client::strict_business(&payload, "触发登录活跃")
}
pub fn attach(mut claim: Value, reward: Value) -> Value {
    if reward["granted"] == true {
        if let Some(points) = util::integer(&reward["points"]).filter(|points| *points > 0) {
            let message = util::text(&claim, &["msg"]);
            if !message.is_empty() {
                claim["msg"] = json!(format!("{message}（活动福利 +{points} 已到账）"));
            }
        }
    }
    claim["promotionReward"] = reward;
    claim
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_active_and_weekly_perks_entries() {
        assert!(is_reward(&json!({ "metadata": { "event_type": "autoclaw_weekly_perks_20261123" } })));
        assert!(is_reward(&json!({ "metadata": { "activity_id": "autoclaw_weekly_perks_20261013" } })));
        assert!(is_reward(&json!({ "metadata": { "activity_id": "autoclaw_user_active_reward_202609" } })));
        assert!(!is_reward(&json!({ "metadata": { "event_type": "daily_signin", "activity_id": "daily_signin" } })));
    }
}
