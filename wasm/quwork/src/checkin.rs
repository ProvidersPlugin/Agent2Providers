//! 每日奖励以 next_refresh_at 为周期身份：先查后领、提前记confirming、随后回查。
//! 未知结果只回查，不重复POST；明确拒绝才允许之后重试，不将未知当作成功。

use super::{account, client, endpoints, ledger, refresh};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

pub const KEY: &str = "quworkDailyReward";

pub struct Status {
    pub claimed: bool,
    pub can_claim: bool,
    pub points: f64,
    pub streak: u32,
    pub reset: i64,
}

impl Status {
    fn parse(value: &Value) -> Result<Self> {
        let invalid = || Error::http(502, "QuWork 每日奖励状态格式异常，未提交领取");
        Ok(Self {
            claimed: value["claimed"].as_bool().ok_or_else(invalid)?,
            can_claim: value["can_claim"].as_bool().ok_or_else(invalid)?,
            points: client::number(&value["points"]).ok_or_else(invalid)?,
            streak: value["consecutive_days"]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(invalid)?,
            reset: client::timestamp(&value["next_refresh_at"]).ok_or_else(invalid)?,
        })
    }
}

pub fn status() -> Result<Status> {
    Status::parse(&refresh::protected("GET", endpoints::DAILY_STATUS, None)?.data("每日奖励查询")?)
}

fn status_with(access: &str) -> Result<Status> {
    Status::parse(
        &client::request("GET", endpoints::DAILY_STATUS, access, None)?.data("每日奖励查询")?,
    )
}

pub fn saved(status: &Status) -> Result<String> {
    let stored = ledger::get(KEY)?;
    Ok(if stored.value["resetAt"].as_i64() == Some(status.reset) {
        match stored.value["state"].as_str() {
            Some("claimed") => "claimed",
            Some("confirming") => "confirming",
            _ => "",
        }
    } else {
        ""
    }
    .to_string())
}

fn report(outcome: &str, status: Option<&Status>, message: &str, points: Option<f64>) -> Value {
    json!({ "outcome": outcome, "message": message, "credit": points.map(|points| format!("{points} 积分")),
        "streak": status.map(|status| status.streak) })
}

fn failure(error: Error) -> Value {
    let outcome = match error.code.as_deref() {
        Some("usage_not_configured") => "ineligible",
        Some("quwork_captcha") => "captcha",
        _ => "failed",
    };
    report(outcome, None, &error.message, None)
}

pub fn run() -> Result<Value> {
    run_for(None)
}

pub fn run_for(expected_reset: Option<i64>) -> Result<Value> {
    Ok(match attempt(expected_reset) {
        Ok(value) => value,
        Err(error) => failure(error),
    })
}

fn attempt(expected_reset: Option<i64>) -> Result<Value> {
    // 第一次读取允许按原协议续期后重试GET；写入前在账号锁内再读一次状态。
    let preflight = status()?;
    if expected_reset.is_some_and(|reset| reset != preflight.reset) {
        return Ok(report(
            "inactive",
            Some(&preflight),
            "QuWork 奖励周期已变化，请刷新活动后重新选择",
            None,
        ));
    }
    let _lock = AccountLock::acquire()?;
    let record = sdk::account()?;
    let secret = refresh::snapshot()?;
    account::unchanged(&record)?;
    let access = account::text(&secret, &["accessToken"]);
    if access.is_empty() {
        return Err(super::errors::missing_account());
    }
    let before = status_with(&access)?;
    account::unchanged(&record)?;
    if expected_reset.is_some_and(|reset| reset != before.reset) {
        return Ok(report(
            "inactive",
            Some(&before),
            "QuWork 奖励周期已变化，请刷新活动后重新选择",
            None,
        ));
    }
    let stored = ledger::get(KEY)?;
    let saved = if stored.value["resetAt"].as_i64() == Some(before.reset) {
        stored.value["state"].as_str().unwrap_or("")
    } else {
        ""
    };
    if before.claimed || saved == "claimed" {
        if before.claimed {
            ledger::save(
                KEY,
                &stored,
                json!({ "resetAt": before.reset, "state": "claimed" }),
            )?;
        }
        return Ok(report("done", Some(&before), "QuWork 今日奖励已领取", None));
    }
    if !before.can_claim || before.reset <= sdk::now() {
        return Ok(report(
            "inactive",
            Some(&before),
            "QuWork 当前没有可领取的每日奖励",
            None,
        ));
    }
    if saved == "confirming" {
        return Ok(report(
            "inactive",
            Some(&before),
            "QuWork 上次领取结果待确认，请稍后查询；不会重复提交领取",
            None,
        ));
    }
    ledger::save(
        KEY,
        &stored,
        json!({ "resetAt": before.reset, "state": "confirming", "at": sdk::now() }),
    )?;
    account::unchanged(&record)?;
    let response = client::request("POST", endpoints::DAILY_CLAIM, &access, None);
    account::unchanged(&record)?;
    if let Ok(reply) = &response {
        if let Ok(data) = reply.data("每日奖励领取") {
            if data["granted"] == true {
                let points = client::number(&data["points"]);
                let granted = Status {
                    streak: data["consecutive_days"]
                        .as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .unwrap_or(before.streak),
                    ..before
                };
                ledger::remember(KEY, json!({ "resetAt": granted.reset, "state": "claimed" }))?;
                return Ok(report(
                    "claimed",
                    Some(&granted),
                    "QuWork 每日奖励领取成功",
                    points,
                ));
            }
        }
    }
    if let Ok(after) = status_with(&access) {
        account::unchanged(&record)?;
        if after.claimed && after.reset == before.reset {
            ledger::remember(KEY, json!({ "resetAt": before.reset, "state": "claimed" }))?;
            return Ok(report(
                "done",
                Some(&after),
                "QuWork 已确认今日奖励到账",
                None,
            ));
        }
    }
    account::unchanged(&record)?;
    if let Ok(reply) = response {
        if reply.rejected() {
            ledger::remember(KEY, json!({ "resetAt": before.reset, "state": "rejected" }))?;
            return Ok(failure(
                reply
                    .data("每日奖励领取")
                    .err()
                    .unwrap_or_else(|| Error::http(502, "QuWork 每日奖励未领取")),
            ));
        }
    }
    Ok(report(
        "inactive",
        Some(&before),
        "QuWork 领取结果待确认，请稍后查询；不会重复提交领取",
        None,
    ))
}
