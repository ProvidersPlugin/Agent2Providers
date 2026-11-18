//! 签到=今天奖励结算，不代替套餐开通/协议确认；日台账与奖励台账均保持旧键名。
//! 领取响应可能重复带回旧活动，先读账本再按同活动剩余增量计入本次收益，不把旧余额当新领取。

use super::{control, ledger, models, subscription, util};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope, Snapshot},
    Error, Result, Value,
};

const ATTEMPT: &str = "bonus-claim-attempt";

fn attempt() -> Result<Snapshot> {
    let value = state::get(Scope::Account, ATTEMPT, 1)?;
    if !value.compatible {
        return Err(uncertain());
    }
    Ok(value)
}

fn put_attempt(old: &Snapshot, value: Value) -> Result<()> {
    if !state::compare_exchange(Scope::Account, ATTEMPT, 1, old.revision, value, 0)? {
        return Err(uncertain());
    }
    Ok(())
}

fn uncertain() -> Error {
    Error::http(
        409,
        "OfficeAce 奖励结算结果未确认，未自动重复提交；请先查询官方积分与今日签到状态",
    )
    .with_code("officeace_bonus_uncertain")
}

pub fn attempted() -> Result<bool> {
    let context = super::account::snapshot()?.1;
    let value = attempt()?.value;
    Ok(value["day"] == util::day(sdk::now())
        && value["binding"] == models::binding(&context)
        && matches!(value["status"].as_str(), Some("submitting" | "uncertain")))
}

pub fn claim() -> Result<Value> {
    let mut context = control::Context::acquire()?;
    let result = inner(&mut context);
    match result {
        Ok(mut value) => {
            context.verify()?;
            if ledger::daily(
                true,
                value["gained"].as_f64().unwrap_or(0.0),
                value["msg"].as_str().unwrap_or("已签到"),
            )
            .is_err()
            {
                util::warning(&mut value, "奖励结算已确认，但签到台账未能保存");
            }
            Ok(value)
        }
        Err(error) => {
            if context.verify().is_ok() {
                let _ = ledger::daily(false, 0.0, &error.message);
            }
            Err(error)
        }
    }
}

fn inner(context: &mut control::Context) -> Result<Value> {
    let today = ledger::today()?;
    if today["all"] == true {
        context.verify()?;
        return Ok(
            json!({ "success": true, "msg": "今天已通过本网关结算过全部奖励", "gained": 0, "rewards": [] }),
        );
    }
    let before = context.subscription()?;
    match before["subscribe_status"].as_str() {
        Some("UNSUBSCRIBED") => {
            return Err(Error::http(
                409,
                "果办尚未开通体验套餐，请先在签到中心确认零元体验套餐，再领取奖励",
            )
            .with_code("officeace_subscription_required"))
        }
        Some("EXPIRED" | "FROZEN") => {
            return Err(Error::http(
                409,
                "果办套餐已到期或被冻结，请在官方客户端处理；这里不会自动续费",
            ))
        }
        Some("SUBSCRIBED") => {}
        _ => {
            return Err(Error::http(
                502,
                "无法确认果办套餐状态，未执行签到，请先查询套餐",
            ))
        }
    }
    let old_bonuses = subscription::bonuses(&before);
    if before["bonus_skus"]
        .as_array()
        .is_none_or(|rows| rows.len() != old_bonuses.len())
    {
        return Err(Error::http(
            502,
            "领取前的奖励账本无法确认，未提交重复结算请求",
        ));
    }
    if subscription::checked_in_today(&before) {
        context.verify()?;
        return Ok(
            json!({ "success": true, "msg": "官方已确认今日奖励到账，无需重复结算", "gained": 0, "rewards": [] }),
        );
    }
    context.verify()?;
    let mark = attempt()?;
    let binding = models::binding(&context.credential);
    let day = util::day(sdk::now());
    if mark.value["day"] == day && mark.value["binding"] == binding {
        match mark.value["status"].as_str() {
            Some("confirmed") => {
                return Ok(
                    json!({ "success": true, "msg": "今天的奖励结算已确认", "gained": 0, "rewards": [] }),
                )
            }
            Some("submitting" | "uncertain") => return Err(uncertain()),
            _ => {}
        }
    }
    context.resolve_project()?;
    context.verify()?;
    put_attempt(
        &mark,
        json!({ "day": day, "binding": binding, "status": "submitting", "at": sdk::now() }),
    )?;
    let reserved = attempt()?;
    let claimed = match context.bonus() {
        Ok(value) => value,
        Err(error) => {
            // 只有确定在出站前拒绝或明确上游拒绝才能重试；传输/回读不明继续保留一次提交。
            if super::errors::permission(&error)
                || error.code.as_deref() != Some(super::errors::UNCERTAIN)
                    && error.code.as_deref() != Some("officeace_account_changed")
            {
                let _ = put_attempt(&reserved, Value::Null);
            }
            return Err(error);
        }
    };
    let items = claimed["bonus_skus"]
        .as_array()
        .ok_or_else(|| Error::http(502, "官方没有返回有效奖励结算结果，请先查询积分"))?;
    let bonuses = subscription::bonuses(&claimed);
    let mut rewards = Vec::new();
    let mut gained = 0.0;
    if items.len() != bonuses.len() {
        return Err(Error::http(
            502,
            "官方奖励条目格式未知，请先查询明细，不要重复提交",
        ));
    }
    for bonus in &bonuses {
        let after = bonus
            .remaining()
            .ok_or_else(|| Error::http(502, "官方奖励余额未能确认，请先查询积分明细"))?;
        let previous = old_bonuses.iter().find(|old| old.id == bonus.id);
        let delta = match previous {
            Some(previous) => {
                after
                    - previous
                        .remaining()
                        .ok_or_else(|| Error::http(502, "领取前奖励余额未知，无法确认新增积分"))?
            }
            None => after,
        }
        .max(0.0);
        gained += delta;
        if delta > 0.0 {
            rewards.push(json!({ "key": bonus.id, "title": bonus.name, "ok": true, "already": false, "gained": delta }));
        }
    }
    if !gained.is_finite() {
        return Err(Error::http(502, "官方奖励积分读数无效，请查询后再操作"));
    }
    let ids: Vec<String> = bonuses.iter().map(|bonus| bonus.id.clone()).collect();
    let message = if gained > 0.0 {
        format!("已领取 {gained:.0} 积分")
    } else {
        "今天没有可领的奖励（或已领过）".into()
    };
    let mut value =
        json!({ "success": true, "msg": message, "gained": gained, "rewards": rewards });
    context.verify()?;
    if put_attempt(
        &reserved,
        json!({ "day": day, "binding": binding, "status": "confirmed", "at": sdk::now() }),
    )
    .is_err()
    {
        util::warning(
            &mut value,
            "奖励结算已确认，但一次提交状态未能保存；请先查询再操作",
        );
    }
    if ledger::mark_claimed(&ids).is_err() {
        util::warning(&mut value, "奖励已由官方确认到账，但本地奖励台账未保存");
    }
    Ok(value)
}

pub fn report() -> Result<Value> {
    let value = claim()?;
    let gained = value["gained"].as_f64().unwrap_or(0.0);
    let mut report = json!({ "outcome": if gained > 0.0 { "claimed" } else { "done" },
        "credit": if gained > 0.0 { Some(format!("{gained:.0} 积分")) } else { None },
        "streak": null, "message": value["msg"] });
    if let Some(warnings) = value.get("warnings") {
        report["warnings"] = warnings.clone();
    }
    Ok(report)
}
