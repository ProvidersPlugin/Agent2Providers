//! 国内每日签到只领当期 daily Credits 活动（每天 10:00 更新），不领取 Sonus 等其它福利。
//! Free 账号没有活动是 inactive；不会调用停用的 daily-check-in 或任何国际组件。
use super::{campaigns, claims, util};
use agent2api_wasm_sdk::{json, Result, Value};

pub fn claim() -> Result<Value> {
    let session = campaigns::load()?;
    let now = util::now()?;
    let rows = session.list["campaigns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| campaigns::daily(row));
    let mut target = None;
    let mut done = false;
    let mut done_until = None;
    for row in rows {
        match campaigns::status(row, now) {
            "claimable" if target.is_none() => {
                target = row["campaignId"].as_str().map(str::to_string)
            }
            "claimed" => {
                done = true;
                done_until = util::timestamp(&row["endAt"]);
            }
            _ => {}
        }
    }
    let Some(id) = target else {
        return Ok(json!({ "outcome": if done { "done" } else { "inactive" },
            "message": if done { "本期每日权益已领取" } else { "当前没有可领取的签到活动" },
            "credit": null, "streak": null, "retryAt": null, "until": done_until }));
    };
    let value = claims::claim(session, &id, true)?;
    let outcome = value["outcome"].as_str().unwrap_or("failed");
    let reward = util::number(&value["campaign"]["amount"]).filter(|amount| *amount > 0.0);
    let message = match outcome {
        "claimed" => reward
            .map(|amount| format!("领取成功，获得 {amount} credits"))
            .unwrap_or_else(|| "每日权益领取成功".into()),
        "already_claimed" => "本期每日权益已领取".into(),
        "confirming" => "上次领取结果仍待确认，请稍后查询，未重复提交".into(),
        "inactive" | "ineligible" => "当前没有可领取的签到活动".into(),
        _ => "本次签到未完成，请稍后重试".into(),
    };
    Ok(
        json!({ "outcome": match outcome { "claimed" => "claimed", "already_claimed" => "done",
            "confirming" => "failed", "inactive" | "ineligible" => "inactive", _ => "failed" },
        "message": message, "credit": if outcome == "claimed" { reward.map(|amount| format!("{amount} credits")) } else { None },
        "streak": null, "retryAt": if outcome == "confirming" { Some(now + 600_000) } else { None },
        "until": value["campaign"]["endsAt"], "details": value }),
    )
}
