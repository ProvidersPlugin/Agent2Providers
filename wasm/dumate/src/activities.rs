//! 成长任务只读，不调用 task/complete 伪报执行；积分、会员奖品由上游自动入账。
//! draw 消耗机会，prize/claim 涉及实物联系方式，均不纳入每日自动签到或代领。

use agent2api_wasm_sdk::{json, Result, Value};

use crate::{account, checkin, endpoints, util};

fn daily(status: &checkin::Status, saved: &str) -> Value {
    json!({ "campaignId": format!("daily:{}", status.day), "title": "百度搭子每日登录积分",
        "titleEn": "DuMate daily sign-in credits",
        "description": "与每日签到共用记录，先查后领；抽奖和成长任务请在官方页面自行操作。",
        "descriptionEn": "Shares the daily check-in ledger. Complete growth tasks and lotteries on the official site.",
        "amount": null, "unit": "积分",
        "status": if status.issued { "claimed" } else if matches!(saved, "confirming" | "claimed") { "confirming" } else { "claimable" },
        "startsAt": null, "endsAt": util::day_start(agent2api_wasm_sdk::now()) + 86_400_000,
        "claimedAt": null, "expiresAt": null, "validDays": null })
}

pub fn preview() -> Result<Value> {
    let current = account::current()?;
    let status = checkin::Status::query(&current)?;
    Ok(
        json!({ "campaigns": [daily(&status, &checkin::saved(&current, &status)?)],
        "riskNote": "这里只领取每日登录积分，不自动抽奖、兑换实物或代做成长任务。",
        "notice": "登录积分可能已由官方自动发放，以官方今日状态为准。", "officialUrl": format!("{}/app", endpoints::WEB) }),
    )
}

pub fn claim(id: &str) -> Result<Value> {
    let day = id.strip_prefix("daily:").unwrap_or("");
    if chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").is_err()
        || day != util::date(util::now()?)?
    {
        return Ok(
            json!({ "campaignId": id, "outcome": "inactive", "message": "该百度搭子活动不可由插件领取" }),
        );
    }
    let result = checkin::claim_for(Some(day))?;
    let outcome = match result["outcome"].as_str() {
        Some("claimed") => "claimed",
        Some("done") => "already_claimed",
        Some("inactive") => "confirming",
        _ => "failed",
    };
    Ok(
        json!({ "campaignId": id, "outcome": outcome, "message": result["message"], "campaign": null }),
    )
}

pub fn tasks() -> Result<Value> {
    account::current()?.web("GET", endpoints::TASKS, None)
}

pub fn lottery() -> Result<Value> {
    account::current()?.web("GET", endpoints::DRAW_STATUS, None)
}
