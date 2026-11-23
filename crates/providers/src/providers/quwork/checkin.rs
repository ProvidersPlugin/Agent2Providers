//! 每日奖励先查后领；未知结果记录到下一次上游重置，只回查、不重复提交。

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use reqwest::Method;
use serde_json::{json, Value};

use super::{accounts, client, endpoints, refresh};
use crate::errors::GatewayError;

const LEDGER: &str = "quworkDailyReward";

#[derive(Clone)]
pub struct Status {
    pub claimed: bool,
    pub can_claim: bool,
    pub points: f64,
    pub streak: u32,
    pub reset_at: i64,
}

pub async fn status(ctx: &PluginCtx, account: &str) -> Result<Status, GatewayError> {
    let data = refresh::protected(
        ctx,
        account,
        Method::GET,
        endpoints::DAILY_STATUS_PATH,
        None,
    )
    .await?
    .data("每日奖励查询")?;
    let invalid = || GatewayError::with_status(502, "QuWork 每日奖励状态格式异常，未提交领取");
    Ok(Status {
        claimed: data
            .get("claimed")
            .and_then(Value::as_bool)
            .ok_or_else(invalid)?,
        can_claim: data
            .get("can_claim")
            .and_then(Value::as_bool)
            .ok_or_else(invalid)?,
        points: client::number(data.get("points")).ok_or_else(invalid)?,
        streak: data
            .get("consecutive_days")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(invalid)?,
        reset_at: client::timestamp(data.get("next_refresh_at")).ok_or_else(invalid)?,
    })
}

pub async fn saved_state(
    ctx: &PluginCtx,
    account: &str,
    status: &Status,
) -> Result<&'static str, GatewayError> {
    let saved = ctx
        .ledger_get(account, LEDGER)
        .await
        .map_err(|_| GatewayError::with_status(500, "QuWork 领取记录读取失败，未提交领取"))?;
    if saved.get("resetAt").and_then(Value::as_i64) != Some(status.reset_at) {
        return Ok("");
    }
    Ok(match saved.get("state").and_then(Value::as_str) {
        Some("confirming") => "confirming",
        Some("claimed") => "claimed",
        _ => "",
    })
}

fn report(
    outcome: proto::CheckinOutcome,
    status: Option<&Status>,
    message: &str,
    points: Option<f64>,
) -> proto::CheckinReport {
    proto::CheckinReport {
        outcome,
        credit: points.map(|points| format!("{points} 积分")),
        streak: status.map(|status| status.streak),
        message: Some(message.to_string()),
    }
}

fn error_report(error: GatewayError) -> proto::CheckinReport {
    let needs_verification = ["验证码", "人机", "captcha"]
        .iter()
        .any(|marker| error.message.contains(marker));
    let outcome = if needs_verification {
        proto::CheckinOutcome::Captcha
    } else if error.code.as_deref() == Some(crate::errors::USAGE_NOT_CONFIGURED_CODE) {
        proto::CheckinOutcome::Ineligible
    } else {
        proto::CheckinOutcome::Failed
    };
    report(outcome, None, &error.message, None)
}

pub async fn run(ctx: &PluginCtx, account: &str) -> Result<proto::CheckinReport, String> {
    let stored = ctx
        .store_get(endpoints::PROVIDER, account)
        .await
        .map_err(|error| error.to_string())?;
    crate::flight::run(&format!("quwork-daily:{}", stored.id), || async {
        Ok(match run_locked(ctx, &stored.id).await {
            Ok(result) => result,
            Err(error) => error_report(error),
        })
    })
    .await
}

async fn run_locked(ctx: &PluginCtx, account: &str) -> Result<proto::CheckinReport, GatewayError> {
    let before = status(ctx, account).await?;
    let saved = saved_state(ctx, account, &before).await?;
    if before.claimed || saved == "claimed" {
        return Ok(report(
            proto::CheckinOutcome::Done,
            Some(&before),
            "QuWork 今日奖励已领取",
            None,
        ));
    }
    if !before.can_claim || before.reset_at <= crate::http::now_ms() {
        return Ok(report(
            proto::CheckinOutcome::Inactive,
            Some(&before),
            "QuWork 当前没有可领取的每日奖励",
            None,
        ));
    }
    if saved == "confirming" {
        return Ok(report(
            proto::CheckinOutcome::Inactive,
            Some(&before),
            "QuWork 上次领取结果待确认，请稍后查询；不会重复提交领取",
            None,
        ));
    }
    ctx.ledger_put(
        account,
        LEDGER,
        &json!({ "resetAt": before.reset_at, "state": "confirming", "at": crate::http::now_ms() }),
    )
    .await
    .map_err(|_| GatewayError::with_status(500, "QuWork 领取记录保存失败，未提交领取"))?;
    let secret = refresh::snapshot(ctx, account).await?;
    let access = accounts::text(&secret, &["accessToken"]);
    let response = client::request(
        &client::client(ctx, None),
        Method::POST,
        endpoints::DAILY_CLAIM_PATH,
        &access,
        None,
    )
    .await;
    if let Ok(reply) = &response {
        if let Ok(data) = reply.data("每日奖励领取") {
            if data.get("granted").and_then(Value::as_bool) == Some(true) {
                let points = client::number(data.get("points"));
                let streak = data
                    .get("consecutive_days")
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok())
                    .unwrap_or(before.streak);
                let granted = Status {
                    streak,
                    ..before.clone()
                };
                let _ = ctx
                    .ledger_put(
                        account,
                        LEDGER,
                        &json!({ "resetAt": before.reset_at, "state": "claimed" }),
                    )
                    .await;
                return Ok(report(
                    proto::CheckinOutcome::Claimed,
                    Some(&granted),
                    "QuWork 每日奖励领取成功",
                    points,
                ));
            }
        }
    }
    if let Ok(after) = status(ctx, account).await {
        if after.claimed && after.reset_at == before.reset_at {
            let _ = ctx
                .ledger_put(
                    account,
                    LEDGER,
                    &json!({ "resetAt": before.reset_at, "state": "claimed" }),
                )
                .await;
            return Ok(report(
                proto::CheckinOutcome::Done,
                Some(&after),
                "QuWork 已确认今日奖励到账",
                None,
            ));
        }
    }
    if let Ok(reply) = response {
        let rejected = (400..500).contains(&reply.status) && reply.status != 408
            || ((200..300).contains(&reply.status)
                && reply.body.get("success").is_some()
                && !client::success(reply.body.get("success")));
        if rejected {
            let _ = ctx
                .ledger_put(
                    account,
                    LEDGER,
                    &json!({ "resetAt": before.reset_at, "state": "rejected" }),
                )
                .await;
            return Ok(error_report(
                reply
                    .data("每日奖励领取")
                    .err()
                    .unwrap_or_else(|| GatewayError::with_status(502, "QuWork 每日奖励未领取")),
            ));
        }
    }
    Ok(report(
        proto::CheckinOutcome::Inactive,
        Some(&before),
        "QuWork 领取结果待确认，请稍后查询；不会重复提交领取",
        None,
    ))
}
