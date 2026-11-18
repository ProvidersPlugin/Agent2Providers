//! 旅行每次仅推进一步；旧09/21排程交宿主逐账号派发，领养门槛按日记忆。
//! 领养含协议确认，只有已明确同意才提交；协议同意保存在账号台账，不替用户决定。
use super::{events, GrowthAccount};
use crate::{ledger, util};
use agent2api_wasm_sdk::{self as sdk, json, Value};

pub fn info(account: &GrowthAccount) -> Result<Option<Value>, String> {
    let reply = super::chat_call(account, "GET", "/activity/growth/buddy/info", None, &[])?;
    if !reply.ok() {
        return Err(format!("猫档案查询失败：{}", reply.msg()));
    }
    let buddy = &reply.data()["buddy"];
    if buddy.is_null() || buddy.as_object().is_some_and(|v| v.is_empty()) {
        Ok(None)
    } else if buddy.is_object() {
        Ok(Some(buddy.clone()))
    } else {
        Err("猫档案格式无效".into())
    }
}
pub fn state(account: &GrowthAccount) -> Result<Value, String> {
    let reply = super::chat_call(
        account,
        "GET",
        "/activity/growth/buddy/travel/status",
        None,
        &[],
    )?;
    if !reply.ok() {
        return Err(format!("旅行状态查询失败：{}", reply.msg()));
    }
    Ok(reply.data().clone())
}
fn report(outcome: &str, message: &str, credit: Option<i64>) -> Value {
    json!({ "outcome": outcome, "message": message, "credit": credit })
}
pub fn consent(account: &GrowthAccount, confirmed: bool) -> Result<(), String> {
    if confirmed {
        ledger::put(
            &account.session,
            "buddy-agreement",
            &json!({ "confirmed": true, "at": sdk::now() }),
        )
        .map_err(|e| e.message)?;
    }
    Ok(())
}
pub fn adopt(account: &GrowthAccount) -> Result<Value, String> {
    if info(account)?.is_some() {
        return Ok(report("skip", "已领养，无需重复领养", Some(0)));
    }
    let known = ledger::get(&account.session, "buddy-agreement").map_err(|e| e.message)?;
    if known["confirmed"] != true {
        return Ok(report(
            "skip",
            "领养前需在官方页面阅读协议，并在本次操作明确确认同意",
            Some(0),
        ));
    }
    let previous = ledger::get(&account.session, "buddy-adopt-attempt").map_err(|e| e.message)?;
    let today = util::day(sdk::now());
    if previous["day"] == today {
        return Ok(report("skip", "今日领养门槛已判定未达，明天再试", Some(0)));
    }
    events::report_chat_send(account, false)?;
    account.pause(1500)?;
    let agreement = super::chat_call(
        account,
        "POST",
        "/activity/growth/buddy/agreement",
        Some(&json!({ "agree": true })),
        &[],
    )?;
    if !agreement.ok() {
        return Err(format!("领养协议提交未成功：{}", agreement.msg()));
    }
    account.pause(super::GROWTH_WRITE_GAP_MS)?;
    let reply = super::chat_call(
        account,
        "POST",
        "/activity/growth/buddy/first",
        Some(&json!({})),
        &[],
    )?;
    if reply.ok() {
        let mut value = report("adopt", "领养成功", util::integer(&reply.data()["credit"]));
        value["energy"] = reply.data()["energy"].clone();
        return Ok(value);
    }
    if reply
        .msg()
        .to_ascii_lowercase()
        .contains("first_buddy task not completed")
    {
        ledger::put(
            &account.session,
            "buddy-adopt-attempt",
            &json!({ "day": today }),
        )
        .map_err(|e| e.message)?;
        return Ok(report(
            "skip",
            "今日对话量未达到领养门槛，明天再试",
            Some(0),
        ));
    }
    Err(format!("领养未完成：{}", reply.msg()))
}
pub fn run(account: &GrowthAccount) -> Result<Value, String> {
    let Some(buddy) = info(account)? else {
        return adopt(account);
    };
    let status = state(account)?;
    match status["state"].as_str().unwrap_or("") {
        "arrived" => {
            let id = util::integer(&status["record_id"])
                .filter(|v| *v > 0)
                .ok_or("旅行已到站但缺少记录身份，未领取")?;
            let reply = super::chat_call(
                account,
                "POST",
                "/activity/growth/buddy/travel/claim",
                Some(&json!({ "record_id": id })),
                &[],
            )?;
            if !reply.ok() {
                return Err(format!("旅行领奖失败：{}", reply.msg()));
            }
            Ok(report(
                "claim",
                "旅行到站奖励已领取",
                util::integer(&reply.data()["reward_credit"]),
            ))
        }
        "idle" if status["daily_limit_reached"] == true => {
            Ok(report("skip", "今日已派出过，明天再来", Some(0)))
        }
        "idle" => {
            let reply = super::chat_call(
                account,
                "POST",
                "/activity/growth/buddy/travel/depart",
                Some(&json!({ "location_id": 4 })),
                &[],
            )?;
            if !reply.ok() {
                return Err(format!("旅行派出失败：{}", reply.msg()));
            }
            Ok(report(
                "depart",
                &format!("已派出旅行（{}，古镇客栈）", util::text(&buddy, &["name"])),
                Some(0),
            ))
        }
        "traveling" => Ok(report("skip", "猫猫仍在旅行途中，下一轮再核对", Some(0))),
        _ => Ok(report("skip", "旅行状态未知，未执行写操作", Some(0))),
    }
}
