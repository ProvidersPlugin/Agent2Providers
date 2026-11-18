//! 国际每日活动北京时间 10:00 开窗，不领 Sonus/限定模型等手动福利。
//! 需增强的步骤调用已授权官方组件；缺授权或组件时如实提示，不记为已签到。
use super::{account, campaigns, claims, daily, risk, util};
use agent2api_wasm_sdk::{json, Result, Value};

fn outcome(outcome: &str, message: &str, retry: Option<i64>) -> Value {
    json!({ "outcome": outcome, "message": message, "credit": null, "streak": null, "retryAt": retry })
}
pub fn claim() -> Result<Value> {
    let now = util::now()?;
    let window = daily::start(now);
    if now < window {
        return Ok(outcome(
            "inactive",
            "每日积分活动尚未开窗（北京时间 10:00），到点后重新查询；未记为已签到",
            Some(window),
        ));
    }
    let session = campaigns::load()?;
    let now = util::now()?;
    let rows = session.list["campaigns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| daily::current(row, now));
    let mut target = None;
    let mut done = false;
    for row in rows {
        match campaigns::status(row, now) {
            "claimable" if target.is_none() => {
                target = row["campaignId"].as_str().map(str::to_string)
            }
            "claimed" => done = true,
            _ => {}
        }
    }
    account::unchanged(&session.snapshot)?;
    // 真实上游当期 CLAIMED 可以证明已领；缺风控不能把这种证据抹掉。
    if done && target.is_none() {
        return Ok(outcome("done", "本期每日权益已领取", None));
    }
    if !session.risk_ready {
        let mut value = outcome(
            "inactive",
            session.risk_note.as_deref().unwrap_or(risk::NOTE),
            None,
        );
        value["reason"] = json!(risk::NOT_READY);
        value["details"] = risk::status()?;
        return Ok(value);
    }
    let Some(id) = target else {
        let retry = daily::retry(now);
        return Ok(outcome(
            "inactive",
            if retry.is_some() {
                "上游暂未给该账号下发每日积分活动，将在开窗后一小时内有限重查"
            } else {
                "本次未发现该账号可领取的每日积分活动，今天不再自动重查；未记为已签到"
            },
            retry,
        ));
    };
    let value = claims::claim(session, &id, true)?;
    let result = value["outcome"].as_str().unwrap_or("failed");
    let reward = util::number(&value["campaign"]["amount"]).filter(|amount| *amount > 0.0);
    let message = match result {
        "claimed" => reward
            .map(|amount| format!("领取成功，获得 {amount} credits"))
            .unwrap_or_else(|| "每日权益领取成功".into()),
        "already_claimed" => "本期每日权益已领取".into(),
        "confirming" => "上次领取结果仍待确认，只回查状态，不重复提交".into(),
        "inactive" | "ineligible" => "当前没有可领取的当期每日活动".into(),
        _ => "本次签到未完成，请稍后重试".into(),
    };
    Ok(
        json!({ "outcome": match result { "claimed" => "claimed", "already_claimed" => "done",
            "confirming" | "inactive" | "ineligible" => "inactive", _ => "failed" },
        "message": message, "credit": if result == "claimed" { reward.map(|amount| format!("{amount} credits")) } else { None },
        "streak": null, "retryAt": if result == "confirming" { daily::retry(util::now()?) } else { None }, "details": value }),
    )
}
