//! 先读上游赠送活动时间，再用共用 ZCode 平面的北京时间窗口兜底；国际版不改用部署时区。
use super::{account, activation, claim, claim_attempt, claim_types::Outcome, network};
use agent2api_wasm_sdk::{self as sdk, json, Result, Value};

pub fn run() -> Result<Value> {
    let started = sdk::now();
    let record = account::with_device()?;
    if account::jwt(&record["secret"]).is_empty() {
        return Ok(report(
            "failed",
            "该 ZCode 账号没有套餐 JWT，无法领取每日赠送额度",
            None,
            Value::Null,
        ));
    }
    let _activation = activation::report(&record)?;
    let snapshot = match claim::discover(false, false) {
        Ok(snapshot) => snapshot,
        Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
        Err(error) => {
            return Ok(report(
                "failed",
                &error.message,
                Some(sdk::now() + 600_000),
                Value::Null,
            ))
        }
    };
    let now = sdk::now();
    let (plans, upcoming): (Vec<_>, Vec<_>) = snapshot.plans.iter().partition(|plan| {
        plan.starts_at
            .is_none_or(|at| at.saturating_mul(1000) <= now)
    });
    if plans.is_empty() {
        if let Some(start) = upcoming.iter().filter_map(|plan| plan.starts_at).min() {
            let at = start.saturating_mul(1000);
            let message = format!(
                "赠送套餐活动尚未开始{}",
                network::clock_label(at)
                    .map(|time| format!("（{time}，北京时间）"))
                    .unwrap_or_default()
            );
            return Ok(report(
                "inactive",
                &message,
                Some((at + 60_000).max(now + 600_000)),
                snapshot.daily,
            ));
        }
        let (known, complete, daily) = {
            let ledger = super::claim_state::Ledger::load(&record)?;
            let daily = ledger.daily();
            (
                daily["totalCount"].as_u64().unwrap_or(0) > 0,
                daily["completed"] == true,
                daily,
            )
        };
        if known && complete && snapshot.failures.is_empty() {
            return Ok(report("done", "今日赠送套餐已领取", None, daily));
        }
        if !snapshot.failures.is_empty() {
            return Ok(report(
                "failed",
                "套餐来源查询不完整，不能确认今日领取状态",
                Some(now + 600_000),
                snapshot.daily,
            ));
        }
        if now < network::daily_window(now) {
            return Ok(report(
                "inactive",
                "今日赠送套餐窗口尚未开启（北京时间 19:00 起）",
                Some(network::daily_window(now) + 60_000),
                snapshot.daily,
            ));
        }
        return Ok(report(
            "inactive",
            "当前没有可领取的赠送套餐",
            Some(now + 600_000),
            snapshot.daily,
        ));
    }
    let expected = plans.len();
    let mut results = Vec::new();
    let mut daily = snapshot.daily;
    let mut succeeded = false;
    let mut count = 0;
    let mut failed = false;
    let mut captcha = false;
    let mut retry_at = None;
    for plan in plans {
        if sdk::now() - started > 55_000 {
            results.push(
                json!({ "outcome": "inactive", "message": "本轮领取时间已用完，剩余套餐稍后继续" }),
            );
            retry_at = Some(sdk::now() + 600_000);
            break;
        }
        let attempt = match claim_attempt::execute(&record, plan, None, started + 100_000) {
            Ok(attempt) => attempt,
            Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
            Err(error) => {
                results.push(json!({ "outcome": "failed", "message": error.message }));
                failed = true;
                break;
            }
        };
        let outcome: &Outcome = &attempt.outcome;
        if outcome.claimed {
            succeeded = true;
        }
        if outcome.settled() {
            count += 1;
        } else if outcome.failure == "captcha" {
            captcha = true;
        } else if !matches!(outcome.failure.as_str(), "unavailable" | "quota_exhausted") {
            failed = true;
        }
        if let Some(at) = outcome.retry_at {
            retry_at = Some(retry_at.map_or(at, |current: i64| current.min(at)));
        }
        let stop = matches!(outcome.failure.as_str(), "captcha" | "login_required")
            || outcome.code == 3012;
        daily = attempt.view["dailyCheckin"].clone();
        results.push(attempt.view);
        if stop {
            break;
        }
    }
    let completed = count == expected && daily["completed"] == true;
    let outcome = if completed {
        if succeeded {
            "claimed"
        } else {
            "done"
        }
    } else if failed {
        "failed"
    } else if captcha {
        "captcha"
    } else {
        "inactive"
    };
    let messages = results
        .iter()
        .filter_map(|item| item["message"].as_str())
        .collect::<Vec<_>>()
        .join("；");
    let mut response = report(outcome, &messages, retry_at, daily);
    response["details"]["results"] = json!(results);
    Ok(response)
}

fn report(outcome: &str, message: &str, retry: Option<i64>, daily: Value) -> Value {
    json!({ "outcome": outcome, "message": message, "credit": null, "streak": null,
        "retryAt": retry, "details": { "success": outcome == "claimed", "alreadyCompleted": outcome == "done",
            "outcome": outcome, "msg": message, "dailyCheckin": daily, "retryAt": retry } })
}
