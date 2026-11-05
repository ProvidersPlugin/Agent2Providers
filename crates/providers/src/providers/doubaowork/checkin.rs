//! 每日签到只提交一次；领取回执不是到账证明，最终以官方 checked_in_today 为准。
//! 待确认记忆先于写请求落盘，手动重试、补签和重启后的执行都只回查同一任务。

use std::time::Duration;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{CheckinResult, PluginCtx};
use serde_json::Value;

use super::{api, checkin_state as state, rewards};
use crate::errors::GatewayError;

const CLAIM_PATH: &str = "/alice/ug/campaign/item/reward/claim/";
const CONFIRM_BUDGET: Duration = Duration::from_secs(18);

pub(super) async fn run(ctx: &PluginCtx, account: &str) -> Result<CheckinResult, GatewayError> {
    let _guard = rewards::claim_lock().lock().await;
    let (id, credentials, client) = match api::account(ctx, account, false).await {
        Ok(account) => account,
        Err(error) => {
            state::publish(ctx, account, &state::failed(&error, false)).await;
            return Err(error);
        }
    };
    let mut ledger = match state::Ledger::load(ctx, &id).await {
        Ok(ledger) => ledger,
        Err(error) => {
            state::publish(ctx, &id, &state::failed(&error, true)).await;
            return Err(error);
        }
    };
    let result = execute(ctx, &id, &credentials, &client, &mut ledger).await;
    let view = match &result {
        Ok(result) => result.clone(),
        Err(error) => state::failed(error, ledger.pending.is_some()),
    };
    state::publish(ctx, &id, &view).await;
    result
}

async fn execute(
    ctx: &PluginCtx,
    account: &str,
    credentials: &Value,
    client: &reqwest::Client,
    ledger: &mut state::Ledger,
) -> Result<CheckinResult, GatewayError> {
    if let Some(attempt) = ledger.pending.as_mut() {
        attempt.rounds = attempt.rounds.saturating_add(1);
        ledger.save(ctx, account).await?;
        return reconcile(ctx, account, credentials, client, ledger, false).await;
    }
    let Some((group, value)) = rewards::detail(client, credentials).await? else {
        return Ok(state::result(
            proto::CheckinOutcome::Inactive,
            "inactive",
            "当前账号未下发豆包工作签到活动，无需反复领取",
            0,
            true,
            false,
            None,
        ));
    };
    let mut daily: Vec<_> = rewards::tasks(&value, &group)
        .into_iter()
        .filter(|task| task.daily)
        .collect();
    if daily.len() > 1 {
        return Err(GatewayError::with_status(
            502,
            "豆包工作返回了多个每日签到任务，无法确定领取对象，未提交领取",
        ));
    }
    let Some(task) = daily.pop() else {
        return Ok(state::result(
            proto::CheckinOutcome::Inactive,
            "inactive",
            "当前活动未提供每日签到任务",
            0,
            true,
            false,
            None,
        ));
    };
    ensure_day(&ledger.day)?;
    // 每日任务已有明确的布尔标记，不能用 task_status=3 覆盖 checked_in_today=false。
    if task.checked {
        return Ok(completed(&task, false, None));
    }
    let Some(body) = task.body.as_ref() else {
        return Ok(state::result(
            proto::CheckinOutcome::Inactive,
            "inactive",
            "今日签到任务尚未满足领取条件",
            1,
            true,
            false,
            None,
        ));
    };
    ensure_day(&ledger.day)?;
    let request = api::request(client, credentials, CLAIM_PATH)?.json(body);
    ledger.pending = Some(state::Attempt {
        group: task.group.clone(),
        item: task.key.clone(),
        submitted_at: crate::http::now_ms(),
        rounds: 1,
        points: task.points,
    });
    // 保存失败就不发写请求，防止重启或并发点击丢掉重复提交保护。
    ledger.save(ctx, account).await?;
    ensure_day(&ledger.day)?;
    let response = api::json_response(request, true).await;
    let accepted = match &response {
        Ok(value) => rewards::receipt(value).map(|_| ()),
        Err(error) => Err(error.clone()),
    };
    if let Err(error) = accepted {
        if !confirming(&error) {
            // 明确拒绝可重新尝试；断网、超时、损坏回执均不能证明写请求未生效。
            if rejected(&error) {
                clear(ctx, account, ledger).await;
            }
            return Err(error);
        }
    }
    if let Ok(value) = response {
        if let Some(points) = crate::quota::number(value.pointer("/data/reward_amount")) {
            if let Some(attempt) = ledger.pending.as_mut() {
                attempt.points = Some(points);
            }
        }
    }
    reconcile(ctx, account, credentials, client, ledger, true).await
}

fn confirming(error: &GatewayError) -> bool {
    if matches!(error.status_code, 401 | 403 | 429) {
        return false;
    }
    // 真实账号的上游原文；业务码尚无稳定证据，只匹配这一条，不把普通 5xx 吞成等待。
    error
        .message
        .trim()
        .strip_prefix("豆包工作：")
        .unwrap_or(error.message.trim())
        .trim_end_matches('。')
        == "领取结果确认中，请稍后重试"
}

fn rejected(error: &GatewayError) -> bool {
    error.code.as_deref() == Some("reward_rejected")
        || matches!(error.status_code, 400 | 401 | 403 | 404 | 405 | 422 | 429)
}

async fn reconcile(
    ctx: &PluginCtx,
    account: &str,
    credentials: &Value,
    client: &reqwest::Client,
    ledger: &mut state::Ledger,
    submitted_now: bool,
) -> Result<CheckinResult, GatewayError> {
    let attempt = ledger
        .pending
        .clone()
        .ok_or_else(|| GatewayError::with_status(500, "豆包工作缺少待确认任务，未提交领取"))?;
    let deadline = tokio::time::Instant::now() + CONFIRM_BUDGET;
    let mut queried = false;
    for delay in [0, 2, 4] {
        if delay > 0 {
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
        ensure_day(&ledger.day)?;
        let value = match tokio::time::timeout_at(
            deadline,
            rewards::campaign_detail(client, credentials, &attempt.group),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                return Err(GatewayError::with_status(
                    504,
                    "豆包工作签到结果查询超时；领取记录已保留，再次点击只查询结果",
                ))
            }
        };
        ensure_day(&ledger.day)?;
        let after = rewards::tasks(&value, &attempt.group)
            .into_iter()
            .find(|task| task.daily && task.key == attempt.item && task.group == attempt.group)
            .ok_or_else(|| {
                GatewayError::with_status(
                    502,
                    "豆包工作回查未返回原签到任务，尚未确认到账；再次点击只查询结果",
                )
            })?;
        queried = true;
        if after.checked {
            let result = completed(&after, submitted_now, attempt.points);
            clear(ctx, account, ledger).await;
            return Ok(result);
        }
        // 超过确认窗口后，用户仍可手动查一次，但不再连查或安排自动重试。
        if attempt.retry_at(crate::http::now_ms()).is_none() {
            break;
        }
    }
    Ok(state::waiting(&attempt, queried))
}

fn completed(task: &rewards::Task, submitted_now: bool, points: Option<f64>) -> CheckinResult {
    let (outcome, status, message) = if submitted_now {
        (
            proto::CheckinOutcome::Claimed,
            "claimed",
            "豆包工作签到奖励已领取，已回查确认",
        )
    } else {
        (
            proto::CheckinOutcome::Done,
            "done",
            "今日已领取豆包工作签到奖励，已由官方确认",
        )
    };
    let mut result = state::result(outcome, status, message, 1, true, false, None);
    result.report.streak = task.streak;
    if submitted_now {
        result.report.credit = points
            .or(task.points)
            .map(|points| format!("{points} 额度"));
    }
    result
}

fn ensure_day(day: &str) -> Result<(), GatewayError> {
    if crate::beijing::today_key() != day {
        return Err(GatewayError::with_status(
            409,
            "签到确认已跨过北京时间零点，未将昨日结果计为今日签到，请重新查询今日活动",
        ));
    }
    Ok(())
}

async fn clear(ctx: &PluginCtx, account: &str, ledger: &mut state::Ledger) {
    if let Err(error) = ledger.clear(ctx, account).await {
        crate::logging::verbose("[DoubaoWork]", &format!("清除签到待确认记录失败：{error}"));
    }
}
