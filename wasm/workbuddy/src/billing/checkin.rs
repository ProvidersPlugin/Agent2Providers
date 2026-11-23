//! 国内每日签到与成长活跃不同；仅明确已领的400/409放行，未知不能伪装成功。
use crate::{
    account, headers, ledger, network,
    session::{self, Session},
    util,
};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

fn already(message: &str) -> bool {
    !["可能", "未能", "失败"].iter().any(|v| message.contains(v))
        && (message.contains("已签到") || message.contains("已领取"))
}
pub fn normalize(data: &Value) -> Value {
    if !data.is_object() {
        return data.clone();
    }
    let pick = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| data.get(*key).filter(|v| !v.is_null()))
            .cloned()
    };
    let int = |keys: &[&str]| {
        pick(keys).and_then(|v| {
            if let Some(b) = v.as_bool() {
                Some(i64::from(b))
            } else {
                util::integer(&v)
            }
        })
    };
    let mut value = json!({ "continuousDays": int(&["continuous_days", "continuousDays", "continuous_checkin_days", "streak_days", "streakDays", "streak", "serial_days"]),
        "totalDays": int(&["total_days", "totalDays", "total_checkin_days", "accumulate_days"]),
        "points": int(&["points", "credit", "reward_points", "rewardPoints", "daily_points"]), "raw": data });
    for (target, aliases) in [
        (
            "checkedIn",
            &[
                "checked_in",
                "checkedIn",
                "is_checked_in",
                "isCheckedIn",
                "signed",
                "is_signed",
                "today_checked_in",
                "todayCheckedIn",
            ][..],
        ),
        ("startTime", &["start_time", "startTime"][..]),
        ("endTime", &["end_time", "endTime"][..]),
        (
            "online",
            &["activity_online_status", "activityOnlineStatus", "online"][..],
        ),
    ] {
        if let Some(v) = pick(aliases) {
            value[target] = v;
        }
    }
    value
}
pub fn status(current: &Session) -> Result<Value> {
    let reply = network::raw(
        "POST",
        &current.endpoint()?,
        "/v2/billing/meter/checkin-activity-status",
        headers::framework(&current.secret)?,
        Some(&json!({})),
    )?;
    if !(200..300).contains(&reply.status) {
        return Err(crate::errors::upstream(
            reply.status,
            &reply.value,
            &current.secrets(),
        ));
    }
    let data = reply.checked(&current.secrets())?;
    if !data.is_object() {
        return Err(Error::http(502, "WorkBuddy签到状态缺少资料，状态未知"));
    }
    Ok(normalize(&data))
}
pub fn claim() -> Result<Value> {
    let _ = session::ensure(false)?;
    let lock = AccountLock::acquire()?;
    let current = session::ensure_locked(&lock)?;
    if !account::personal(&current.secret) {
        return Ok(
            json!({ "outcome": "ineligible", "success": false, "message": "企业账号没有个人每日签到" }),
        );
    }
    let today = util::day(sdk::now());
    let old = ledger::get(&current, "dailyCheckin")?;
    if old["day"] == today && old["completed"] == true {
        let mut report = old["report"].clone();
        report["outcome"] = json!("done");
        report["alreadyCompleted"] = json!(true);
        return Ok(report);
    }
    let reply = network::raw(
        "POST",
        &current.endpoint()?,
        "/v2/billing/meter/daily-checkin",
        headers::framework(&current.secret)?,
        Some(&json!({})),
    )?;
    let message = crate::errors::clean(reply.message(), &current.secrets());
    let duplicate = (reply.code() == Some(1001) || already(&message))
        && (matches!(reply.status, 400 | 409) || (200..300).contains(&reply.status));
    let ineligible = reply.code() == Some(1002);
    let inactive = reply.code() == Some(1003)
        || message.contains("活动未开启")
        || message.contains("无此活动");
    if !(200..300).contains(&reply.status) && !duplicate && !ineligible && !inactive {
        return Err(crate::errors::upstream(
            reply.status,
            &reply.value,
            &current.secrets(),
        ));
    }
    let success = reply.ok() && reply.code() == Some(0) && reply.data().is_object() && !duplicate;
    let data = normalize(reply.data());
    let outcome = if success {
        "claimed"
    } else if duplicate {
        "done"
    } else if ineligible {
        "ineligible"
    } else if inactive {
        "inactive"
    } else {
        "failed"
    };
    let mut report = json!({ "outcome": outcome, "success": success, "alreadyCompleted": duplicate,
        "code": reply.code(), "data": data, "msg": message, "message": if success { "签到成功" } else { &message },
        "credit": util::integer(&data["points"]).filter(|v| *v > 0).map(|v| format!("+{v} 积分")), "streak": data["continuousDays"] });
    session::check(&current)?;
    if ledger::put(&current, "dailyCheckin", &json!({ "day": today, "at": sdk::now(), "completed": success || duplicate, "report": report })).is_err() {
        util::warning(&mut report, "签到结果已返回，但每日台账未保存");
    }
    Ok(report)
}
