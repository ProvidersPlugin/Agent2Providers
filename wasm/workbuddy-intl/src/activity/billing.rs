//! 国际活动状态路径不带/v2；领奖路径带/v2。未查/失败/金额缺失均为null，不冒充不活动或零到账。
use super::Context;
use crate::{errors, headers, network, session::Session, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

const STATUS: &str = "/billing/meter/checkin-activity-status";
const CLAIM: &str = "/v2/billing/meter/daily-checkin";
fn shape(reply: network::Reply, current: &Session) -> Value {
    let available = reply.ok() && reply.code() == Some(0) && reply.data().is_object();
    let data = reply.data();
    let unavailable = (200..300).contains(&reply.status) && reply.code() == Some(12302);
    let active = if unavailable {
        Some(false)
    } else {
        available.then(|| data["active"].as_bool()).flatten()
    };
    let checked = available
        .then(|| {
            data["today_checked_in"]
                .as_bool()
                .or_else(|| data["todayCheckedIn"].as_bool())
        })
        .flatten();
    let credit = available
        .then(|| {
            util::number(&data["daily_credit"])
                .or_else(|| util::number(&data["dailyCredit"]))
                .filter(|amount| *amount >= 0.0)
        })
        .flatten();
    json!({ "active": active, "todayCheckedIn": checked, "dailyCredit": credit,
        "statusAvailable": available, "code": reply.code(), "status": reply.status,
        "msg": errors::clean(reply.message(), &current.secrets()) })
}
pub fn status_for(current: &Session) -> Result<Value> {
    let reply = network::raw_timeout(
        "POST",
        &current.endpoint()?,
        STATUS,
        headers::framework(&current.secret)?,
        Some(&json!({})),
        20_000,
    )?;
    if super::authentication(&reply) {
        return Err(errors::upstream(
            reply.status,
            &reply.value,
            &current.secrets(),
        ));
    }
    Ok(shape(reply, current))
}
pub(super) fn status(ctx: &mut Context) -> Result<Value> {
    let reply = ctx.call("POST", STATUS, Some(&json!({})))?;
    Ok(shape(reply, &ctx.current))
}
pub(super) fn status_error(error: Error) -> Value {
    json!({ "active": null, "todayCheckedIn": null, "dailyCredit": null, "statusAvailable": false,
        "code": error.upstream_code, "status": error.status, "msg": error.message })
}
fn unknown(message: &str) -> Value {
    json!({ "success": false, "outcome": "unknown", "uncertain": true, "alreadyCompleted": false,
        "code": null, "credit": null, "msg": message })
}
fn remembered(mut result: Value) -> Value {
    if !result.is_object() {
        result = json!({});
    }
    result["success"] = json!(false);
    result["outcome"] = json!("done");
    result["alreadyCompleted"] = json!(true);
    result["msg"] = json!("今日奖励已确认领取，无需重复提交");
    result["credit"] = Value::Null;
    if let Some(fields) = result.as_object_mut() {
        fields.remove("data");
    }
    result
}
fn duplicate(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    !["失败", "未能", "可能", "failed", "unable"]
        .iter()
        .any(|part| lower.contains(part))
        && [
            "已领取",
            "已签到",
            "already claimed",
            "already checked in",
            "already checked-in",
        ]
        .iter()
        .any(|part| lower.contains(part))
}
pub(super) fn claim(ctx: &mut Context, status: &Value) -> Result<Value> {
    let before = ctx.get("claim")?;
    if before["day"] == ctx.day && before["settled"] == true {
        return Ok(remembered(before["result"].clone()));
    }
    if status["statusAvailable"] == true && status["todayCheckedIn"] == true {
        let mut result = remembered(json!({ "code": status["code"] }));
        if ctx
            .put(
                "claim",
                &json!({ "day": ctx.day, "settled": true, "pending": false, "result": result }),
            )
            .is_err()
        {
            util::warning(&mut result, "官方已确认今日领取，但本地结算台账未保存");
        }
        return Ok(result);
    }
    if before["day"] == ctx.day && before["pending"] == true {
        return Ok(unknown(
            "上一轮领奖结果未确认，未再次提交；请先通过活动状态或官方页面核对",
        ));
    }
    if status["active"] == false {
        return Ok(
            json!({ "success": false, "outcome": "inactive", "credit": null, "code": status["code"],
            "msg": "活动未开启，本轮没有可领取奖励" }),
        );
    }
    if status["statusAvailable"] != true
        || status["active"] != true
        || status["todayCheckedIn"] != false
    {
        return Ok(unknown("国际活动状态未知，未领取；请查看探测结果"));
    }
    ctx.check()?;
    ctx.put(
        "claim",
        &json!({ "day": ctx.day, "pending": true, "settled": false }),
    )?;
    let reply = match ctx.call("POST", CLAIM, Some(&json!({}))) {
        Ok(reply) => reply,
        Err(_) => {
            return Ok(unknown(
                "国际领奖请求结果未确认，已保留待核对记录，不自动重发",
            ))
        }
    };
    let message = errors::clean(reply.message(), &ctx.current.secrets());
    let already = matches!(reply.status, 200 | 400 | 409) && duplicate(&message);
    let success = reply.ok() && reply.code() == Some(0) && reply.data().is_object() && !already;
    let certain_rejection = (400..500).contains(&reply.status) && reply.status != 408
        || (200..300).contains(&reply.status) && reply.code().is_some_and(|code| code != 0);
    let uncertain = !success && !already && !certain_rejection;
    let amount = if success {
        util::number(&reply.data()["credit"])
            .or_else(|| util::number(&reply.data()["points"]))
            .filter(|amount| *amount >= 0.0)
    } else {
        None
    };
    let inactive = reply.code() == Some(12302)
        || message.to_ascii_lowercase().contains("activity is offline")
        || message.contains("活动未开启")
        || message.contains("无此活动");
    let mut result = json!({ "success": success, "alreadyCompleted": already, "uncertain": uncertain,
        "outcome": if success { "claimed" } else if already { "done" } else if uncertain { "unknown" } else if inactive { "inactive" } else { "failed" },
        "credit": amount, "code": reply.code(), "status": reply.status,
        "msg": if success { "国际活跃奖励已领取" } else { &message } });
    if success {
        // 金额只投影上游确切字段；不把日活预期30积分填进本次到账。
        result["data"] = json!({ "credit": amount, "points": util::number(&reply.data()["points"]).filter(|value| *value >= 0.0),
            "continuousDays": util::integer(&reply.data()["continuous_days"]) });
    }
    if ctx
        .put(
            "claim",
            &json!({ "day": ctx.day, "pending": uncertain, "settled": success || already,
        "result": result }),
        )
        .is_err()
    {
        util::warning(
            &mut result,
            "国际领奖结果已返回，但台账未保存；下次先核对官方状态",
        );
    }
    Ok(result)
}
