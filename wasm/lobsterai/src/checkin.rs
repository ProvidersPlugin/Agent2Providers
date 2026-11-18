//! 三步签到：活动位 → 上下文两道预检 → check_in。所有写操作之前保存幂等键。
//! 传输／业务结果不明时保留记忆，后续只能回查，不能再生成新键重复提交当天奖励。

use super::{account, login, upstream, version};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{Scope, Snapshot},
    AccountLock, Error, Result, Value,
};

const KEY: &str = "daily-checkin";
const DAY: i64 = 86_400_000;

fn report(outcome: &str, message: &str, activity: &str) -> Value {
    let until = (sdk::now().saturating_add(8 * 3_600_000).div_euclid(DAY) + 1)
        .saturating_mul(DAY)
        .saturating_sub(8 * 3_600_000);
    json!({ "outcome": outcome, "message": message,
        "until": matches!(outcome, "claimed" | "done").then_some(until), "details": {
        "activityCode": activity, "success": outcome == "claimed", "alreadyCompleted": outcome == "done",
        "needsAction": !matches!(outcome, "claimed" | "done") } })
}

fn revision(value: &Value) -> Result<(String, Value)> {
    let revision = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .filter(|value| *value <= 9_007_199_254_740_991)
        .ok_or_else(|| Error::http(502, "LobsterAI 活动缺少有效配置版本，未提交签到"))?;
    Ok((revision.to_string(), json!(revision)))
}

fn save(snapshot: &Snapshot, value: Value) -> Result<()> {
    if !snapshot.compatible
        || !sdk::state::compare_exchange(Scope::Account, KEY, 1, snapshot.revision, value, 0)?
    {
        return Err(Error::http(409, "LobsterAI 签到状态已变化，未重复提交"));
    }
    Ok(())
}

fn remember(day: i64, activity: &str, result: &Value, account: &Value) -> Result<()> {
    account::validate_current(account)?;
    let snapshot = sdk::state::get(Scope::Account, KEY, 1)?;
    save(
        &snapshot,
        json!({ "day": day, "activity": activity, "phase": "done", "result": result, "identity": account["identity"] }),
    )
}

fn validate_context(value: &Value) -> Result<()> {
    if !value["state"].is_object()
        || !value["state"]["claimedToday"].is_boolean()
        || value["actions"].as_array().is_none_or(|actions| {
            actions.len() > 64
                || actions.iter().any(|action| {
                    action.as_str().is_none_or(|action| {
                        action.len() > 128 || action.chars().any(char::is_control)
                    })
                })
        })
    {
        return Err(Error::http(
            502,
            "LobsterAI 签到上下文缺少有效领取状态，未提交或重复领取",
        ));
    }
    if let Some(revision_value) = value.get("configRevision") {
        revision(revision_value)?;
    }
    Ok(())
}

fn already(activity: &str) -> Value {
    report("done", "今天已签到", activity)
}

pub fn claim() -> Result<Value> {
    account::ensure()?;
    let client_version = version::resolve();
    let _lock = AccountLock::acquire()?;
    let account = account::snapshot()?;
    let token = account::access(&account["secret"])?;
    let day = sdk::now().saturating_add(8 * 3_600_000).div_euclid(DAY);
    let state = sdk::state::get(Scope::Account, KEY, 1)?;
    if !state.compatible
        || state
            .value
            .get("identity")
            .is_some_and(|id| id != &account["identity"])
    {
        return Err(Error::new("LobsterAI 签到记忆不属于当前账号，请重新核对"));
    }
    // 成功事实不能因为奖励活动从列表消失而退化成“无活动”。
    if state.value["day"].as_i64() == Some(day) && state.value["phase"] == "done" {
        let mut result = already(state.value["activity"].as_str().unwrap_or(""));
        if let Some(credit) = state.value["result"].get("credit") {
            result["credit"] = credit.clone();
        }
        return Ok(result);
    }

    let slot_path = format!("/api/client-activities/slot?placement=desktop_sidebar&clientVersion={}&containerApiVersion=2&platform=win32",
        upstream::query(&client_version));
    let slot = upstream::get(&slot_path, &token, "活动查询")?;
    account::validate_current(&account)?;
    let activity = account::text(&slot["activity"], &["activityCode"]);
    if activity.len() > 512
        || activity.chars().any(char::is_control)
        || slot["slotState"]
            .as_str()
            .is_none_or(|state| state.trim().is_empty())
        || slot["slotState"] == "available"
            && (activity.is_empty() || !slot["activity"].is_object())
    {
        return Err(Error::http(502, "LobsterAI 活动状态格式无效，未提交签到"));
    }
    if slot["slotState"] != "available" || activity.is_empty() {
        if state.value["phase"] == "submitting" {
            return Ok(report(
                "failed",
                "上次签到结果尚未确认，活动状态暂不可查，未重复领取",
                &activity,
            ));
        }
        return Ok(report("inactive", "今天没有可签到的活动", &activity));
    }
    let (revision_query, mut revision_value) = revision(&slot["activity"]["configRevision"])?;
    let context_path = format!(
        "/api/client-activities/{}/context?configRevision={}",
        upstream::query(&activity),
        upstream::query(&revision_query)
    );
    let context = upstream::get(&context_path, &token, "活动上下文查询")?;
    account::validate_current(&account)?;
    validate_context(&context)?;
    if let Some(value) = context.get("configRevision") {
        revision_value = revision(value)?.1;
    }
    if context["state"]["claimedToday"] == true {
        let mut result = already(&activity);
        if let Some(credit) =
            account::number(&context["state"]["rewardCredits"]).filter(|credit| *credit >= 0.0)
        {
            result["credit"] = json!(format!("{credit} 积分"));
        }
        remember(day, &activity, &result, &account)?;
        return Ok(result);
    }
    if state.value["phase"] == "submitting" {
        return Ok(report(
            "failed",
            "上次签到结果尚未确认，已回查但未重复领取；请稍后重查或前往官方客户端确认",
            &activity,
        ));
    }
    if !context["actions"].as_array().is_some_and(|actions| {
        actions.iter().any(|action| {
            action
                .as_str()
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("check_in"))
        })
    }) {
        return Ok(report("inactive", "当前活动未开放签到", &activity));
    }
    let idempotency_key = login::uuid()?;
    account::validate_current(&account)?;
    save(
        &state,
        json!({ "day": day, "activity": activity, "phase": "submitting", "identity": account["identity"],
        "idempotency_key": idempotency_key, "submitted_at": sdk::now() }),
    )?;
    account::validate_current(&account)?;
    let body = json!({ "configRevision": revision_value, "idempotencyKey": idempotency_key, "payload": {} });
    let path = format!(
        "/api/client-activities/{}/actions/check_in",
        upstream::query(&activity)
    );
    let posted = upstream::post(&path, &token, &body, "签到");
    account::validate_current(&account)?;
    // 传输或业务结果不明也只能回查；不会再生成新幂等键。
    let after = upstream::get(&context_path, &token, "签到后复核")?;
    account::validate_current(&account)?;
    validate_context(&after)?;
    if after["state"]["claimedToday"] != true {
        return match posted {
            Err(error) => Err(error),
            Ok(_) => Ok(report(
                "failed",
                "签到结果尚未确认，未重复领取；请稍后重新查询",
                &activity,
            )),
        };
    }
    let data = posted.unwrap_or(Value::Null);
    let result = &data["result"];
    let granted = [result, &data, &after["state"]].iter().find_map(|value| {
        ["creditsGranted", "rewardCredits", "credits"]
            .iter()
            .find_map(|key| account::number(&value[*key]).filter(|amount| *amount >= 0.0))
    });
    let message = granted
        .map(|credit| format!("签到已确认，获得 {credit} 积分"))
        .unwrap_or_else(|| "签到已确认，积分可在余额中查看".into());
    let mut outcome = report("claimed", &message, &activity);
    if let Some(credit) = granted {
        outcome["credit"] = json!(format!("{credit} 积分"));
        outcome["details"]["credit"] = json!(credit);
        outcome["details"]["creditGranted"] = json!(true);
    } else {
        outcome["details"]["creditGranted"] = json!(false);
    }
    remember(day, &activity, &outcome, &account)?;
    Ok(outcome)
}
