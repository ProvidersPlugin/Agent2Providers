//! 奖励账本出现即已到账，任务done恒true；settled只表示北京时间今天经本网关结算过。
//! 查询不会自动领奖励，领取与签到共用日台账；领取后重查完整账本。

use super::{checkin, control, ledger, subscription, util};
use agent2api_wasm_sdk::{json, Result, Value};

pub fn status() -> Result<Value> {
    let mut context = control::Context::acquire()?;
    let snapshot = context.subscription()?;
    let bonuses = subscription::bonuses(&snapshot);
    if snapshot["bonus_skus"]
        .as_array()
        .is_none_or(|rows| rows.len() != bonuses.len())
    {
        return Err(agent2api_wasm_sdk::Error::http(
            502,
            "果办奖励账本缺失或格式未知，未当成零条奖励",
        ));
    }
    let total = bonuses
        .iter()
        .map(|bonus| bonus.total)
        .collect::<Option<Vec<_>>>()
        .map(|totals| totals.iter().sum::<f64>())
        .filter(|value| value.is_finite());
    let today = ledger::today()?;
    let official_settled = subscription::checked_in_today(&snapshot);
    let pending = checkin::attempted()? && !official_settled;
    context.verify()?;
    let tasks: Vec<Value> = bonuses
        .iter()
        .map(|bonus| {
            json!({ "key": bonus.id, "title": bonus.name,
        "group": "奖励积分", "points": bonus.total, "done": true, "claimable": false,
        "remaining": bonus.remaining(), "expiresAt": bonus.expires })
        })
        .collect();
    Ok(
        json!({ "tasks": tasks, "earned": total, "total": total, "unclaimed": 0,
        "settled": today["all"] == true || official_settled, "pending": pending, "provider": "officeace",
        "note": "这里列的是已到账奖励账本；一键领取只触发今天的奖励结算，不会开通套餐或自动续费" }),
    )
}

pub fn claim_all() -> Result<Value> {
    let claimed = checkin::claim()?;
    let mut result = status().unwrap_or_else(|_| {
        let mut value = json!({ "provider": "officeace", "settled": true });
        util::warning(
            &mut value,
            "本次奖励结算已确认，但完整账本暂未刷新，请稍后查询",
        );
        value
    });
    let rewards = claimed["rewards"].as_array().cloned().unwrap_or_default();
    result["claimed"] = json!(rewards.len());
    result["results"] = json!(rewards);
    result["claimedPoints"] = claimed["gained"].clone();
    result["failed"] = json!(0);
    result["settled"] = json!(true);
    result["message"] = claimed["msg"].clone();
    for warning in claimed["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        util::warning(&mut result, warning);
    }
    Ok(result)
}

pub fn usage() -> Result<Value> {
    let mut context = control::Context::acquire()?;
    let snapshot = context.subscription()?;
    Ok(super::errors::public_payload(
        &subscription::summarize(&snapshot),
        &context.record["secret"],
    ))
}
