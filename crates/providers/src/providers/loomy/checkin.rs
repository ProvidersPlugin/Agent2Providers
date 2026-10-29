//! Loomy 的「每日签到」= **每日首次登录刷新赠送积分**（已确认）。
//!
//! ── 上游是什么（安装包全包检索 + `points-service.js` 对照）────
//! Loomy **没有独立的签到 / 领取接口**（客户端全包检索「签到 / checkin /
//! 每日任务」零命中）。它每天把「每日赠送积分」刷回 5000 的机制是：
//!
//! ```text
//! POST {集成网关}/api/v1/points/first-login      header: token: <session>
//! body: {}（可带 inviteCode / deviceId / channel，签到场景都不需要）
//!   → {code:"000000", desc, data:{…}}
//! ```
//!
//! 服务端以「当天是否第一次」决定是否把赠送额度刷回 5000 —— 也就是说
//! 「签到」的触发点是登录，而这里把同一个调用接进自动签到框架，让长期不登录
//! 的账号也能每天把赠送积分续上。
//!
//! ── 判成功的口径（与别家对齐）───────────────────────────────
//! HTTP 通了且业务码 `000000` 才算成功。重复调用是**按天幂等**的（服务端当天
//! 已刷过就原样返回成功），所以「今天已刷过」不算失败 —— 与 AutoClaw
//! 「今天已签到 → success:false」不同，这里没有可区分的返回字段，如实报成功
//! 并说明「每日赠送积分已刷新」。

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::client;
use super::credentials;

/// 每日登录刷新路径（相对集成网关）
const FIRST_LOGIN_PATH: &str = "/api/v1/points/first-login";

/// Loomy 账号的每日积分刷新（自动签到框架的 claim）。
pub async fn claim_daily_login(
    ctx: &PluginCtx,
    account: &str,
) -> Result<proto::CheckinReport, String> {
    let client = crate::http::control_client(None, &ctx.env.proxy);
    let credentials = credentials::snapshot(ctx, account).await?;
    let payload = client::token_request(
        &client,
        "POST",
        FIRST_LOGIN_PATH,
        &credentials.session,
        Some(&json!({})),
        "每日积分刷新",
    )
    .await?;

    let code = client::business_code(&payload);
    if client::is_auth_error_code(&code) {
        return Err("Loomy 登录态已失效，无法刷新每日积分（请重新登录）".to_string());
    }
    if code != client::SUCCESS_CODE {
        let message = client::upstream_message(&payload);
        let message = if message.trim().is_empty() {
            format!("每日积分刷新失败（上游 code={code}）")
        } else {
            format!("每日积分刷新失败：{message}")
        };
        agent2api_provider_sdk::log_warn!("[Loomy] ⚠️ {message}");
        return Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Failed,
            credit: None,
            streak: None,
            message: Some(message),
        });
    }

    // 响应里可能带今日的赠送额度（字段名未在客户端硬编码，取到就播报）
    let daily_hint = payload
        .pointer("/data/dailyBalance")
        .and_then(Value::as_i64)
        .or_else(|| payload.pointer("/data/data/dailyBalance").and_then(Value::as_i64));
    let message = match daily_hint {
        Some(balance) => format!("每日赠送积分已刷新（当前 {balance}）"),
        None => "每日赠送积分已刷新".to_string(),
    };
    Ok(proto::CheckinReport {
        outcome: proto::CheckinOutcome::Claimed,
        credit: daily_hint.map(|value| value.to_string()),
        streak: None,
        message: Some(message),
    })
}
