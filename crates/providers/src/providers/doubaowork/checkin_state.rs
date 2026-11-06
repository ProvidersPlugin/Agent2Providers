use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{CheckinResult, PluginCtx};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::errors::GatewayError;

const LEDGER: &str = "doubaowork-checkin";
const VERSION: u8 = 1;
const RETRY_GAP_MS: i64 = 10 * 60 * 1000;
const CONFIRM_WINDOW_MS: i64 = 30 * 60 * 1000;
const MAX_ROUNDS: u32 = 4;

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ClaimEndpoint {
    #[default]
    ItemReward,
    LandingCheckin,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Attempt {
    pub group: String,
    pub item: String,
    #[serde(default)]
    pub endpoint: ClaimEndpoint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_failure: Option<i64>,
    pub submitted_at: i64,
    pub rounds: u32,
    pub points: Option<f64>,
}

impl Attempt {
    pub fn retry_at(&self, now: i64) -> Option<i64> {
        let next = now.saturating_add(RETRY_GAP_MS);
        (self.rounds < MAX_ROUNDS
            && next <= self.submitted_at.saturating_add(CONFIRM_WINDOW_MS)
            && crate::beijing::date_key_of_ms(next) == crate::beijing::date_key_of_ms(now))
        .then_some(next)
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Ledger {
    version: u8,
    pub day: String,
    pub pending: Option<Attempt>,
}

impl Ledger {
    pub async fn load(ctx: &PluginCtx, account: &str) -> Result<Self, GatewayError> {
        let day = crate::beijing::today_key();
        let fresh = || Self {
            version: VERSION,
            day: day.clone(),
            pending: None,
        };
        // 只有明确的空台账才是首次领取；读失败或格式损坏不能抹掉上次提交的记忆。
        let value = ctx.ledger_get(account, LEDGER).await.map_err(|error| {
            GatewayError::with_status(
                502,
                format!("读取豆包工作签到记录失败，未提交领取：{error}"),
            )
        })?;
        if value.is_null() {
            return Ok(fresh());
        }
        let stored: Self = serde_json::from_value(value).map_err(|_| invalid())?;
        let date =
            chrono::NaiveDate::parse_from_str(&stored.day, "%Y-%m-%d").map_err(|_| invalid())?;
        if stored.version != VERSION || date.format("%Y-%m-%d").to_string() != stored.day {
            return Err(invalid());
        }
        if stored.day > day {
            return Err(GatewayError::with_status(
                500,
                "系统日期早于豆包工作签到记录，请先校准时间",
            ));
        }
        if stored.day < day {
            return Ok(fresh());
        }
        if let Some(attempt) = &stored.pending {
            if attempt.group.trim().is_empty()
                || attempt.item.trim().is_empty()
                || attempt.submitted_at <= 0
                || crate::beijing::date_key_of_ms(attempt.submitted_at).as_deref()
                    != Some(&stored.day)
                || attempt
                    .points
                    .is_some_and(|points| !points.is_finite() || points < 0.0)
            {
                return Err(invalid());
            }
        }
        Ok(stored)
    }

    pub async fn save(&self, ctx: &PluginCtx, account: &str) -> Result<(), GatewayError> {
        let value = serde_json::to_value(self).map_err(|_| invalid())?;
        ctx.ledger_put(account, LEDGER, &value)
            .await
            .map_err(|error| {
                GatewayError::with_status(502, format!("保存豆包工作签到记录失败：{error}"))
            })
    }

    pub async fn clear(&mut self, ctx: &PluginCtx, account: &str) -> Result<(), GatewayError> {
        let previous = self.pending.take();
        if let Err(error) = self.save(ctx, account).await {
            self.pending = previous;
            return Err(error);
        }
        Ok(())
    }
}

fn invalid() -> GatewayError {
    GatewayError::with_status(500, "豆包工作签到记录无效，已停止提交领取以避免重复操作")
}

pub(super) fn result(
    outcome: proto::CheckinOutcome,
    status: &str,
    message: impl Into<String>,
    total: u32,
    sources_complete: bool,
    query_only: bool,
    retry_at: Option<i64>,
) -> CheckinResult {
    let message: String = message.into().chars().take(2000).collect();
    let completed = matches!(
        outcome,
        proto::CheckinOutcome::Claimed | proto::CheckinOutcome::Done
    );
    CheckinResult {
        report: proto::CheckinReport {
            outcome,
            credit: None,
            streak: None,
            message: Some(message.clone()),
        },
        retry_at,
        details: Some(json!({"dailyCheckin": {
            "at": crate::http::now_ms(), "completed": completed,
            "completedCount": if completed { total } else { 0 }, "totalCount": total,
            "sourcesComplete": sources_complete, "status": status,
            "message": message, "retryAt": retry_at, "queryOnly": query_only,
        }})),
    }
}

pub(super) fn waiting(attempt: &Attempt, sources_complete: bool) -> CheckinResult {
    let retry_at = attempt.retry_at(crate::http::now_ms());
    let message = if retry_at.is_some() {
        "豆包工作领取结果待确认，稍后只查询结果，不会重复提交领取"
    } else {
        "豆包工作今日领取仍未确认，请前往官方客户端查看；再次点击仅查询结果，不会重复领取"
    };
    result(
        proto::CheckinOutcome::Inactive,
        "confirming",
        message,
        1,
        sources_complete,
        true,
        retry_at,
    )
}

pub(super) fn failed(error: &GatewayError, query_only: bool) -> CheckinResult {
    result(
        proto::CheckinOutcome::Failed,
        "failed",
        error.message.clone(),
        1,
        false,
        query_only,
        None,
    )
}

pub(super) async fn publish(ctx: &PluginCtx, account: &str, result: &CheckinResult) {
    let Some(state) = result
        .details
        .as_ref()
        .and_then(|details| details.get("dailyCheckin"))
    else {
        return;
    };
    // 结果已由官方确认时，展示快照写失败不能把已领取改成失败；下一次仍会先回查。
    if let Err(error) = ctx.ledger_put(account, "dailyCheckin", state).await {
        crate::logging::verbose("[DoubaoWork]", &format!("保存签到展示状态失败：{error}"));
    }
}
