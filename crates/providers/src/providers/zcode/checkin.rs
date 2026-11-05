//! ZCode 的**每日签到** —— 就是「领当天那份赠送套餐」。
//!
//! ── 为什么签到就是领取（链路依据）──────────────────────────
//! ZCode 没有独立的签到接口，但上游每天给账号下发一份**带日期段的新套餐**
//! （`zcode-v3-start-plan-<日期>` / `zcode-v3-start-plan-1024` 这类），用户在
//! 客户端里点一下就领 —— 那一下就是这一家的「签到」。判据与 `claim.rs` 模块头
//! 记的一致：同一期再领回 `1003 already claimed`。手动领取、自动签到与
//! 领取状态预览共用逐套餐台账，已领取后从上游预览消失也不会丢失完成事实。
//!
//! ── 验证与结果 ──────────────────────────────────────────────
//! 手动领取和定时领取使用同一份逐套餐台账。定时领取发现未领套餐后，按领取配置
//! 向已连接的管理界面请求一次官方 SDK 验证，结果只服务本账号的本次领取。
//! 不读取模型调用令牌池，也不把 `skip_model_request` 当成领取免验证。
//! 无可用界面、取消或超时返回 `captcha`，不写已领取、不作为无活动窗口重试。
//! 等待有上限，验证任务结束后迟到的凭据不会被其它领取或模型调用复用。
//!
//! ── 顺序：补报激活 → 探测 → 领取 ───────────────────────────
//! 资格由激活事件（`app_launch` / `app_daily_active`）发放，而本家不跑官方
//! 客户端，那一步不会自动发生（见 `activation.rs`）。所以签到先补报一次激活
//! 再探测，与手动领取那条链同序。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;

use super::region::Region;
use super::{activation, claim};

const ROUND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(100);

/// 每日窗口的开启时刻（北京时间秒偏移）：**19:00**。
///
/// 依据：官方公告「每日 19:00 起可领当日赠送额度，23:00 至次日 09:00 可用」
/// （2026-10 的 ZCode 每日 3 亿 GLM-5.3-Flash 活动）。
///
/// ★ 它现在只是**兜底**：上游会在 `marketing/touch` 的 `zcode_plan` 与
/// `billing/preview` 的 `plans` 里给出每期活动的 `starts_at` / `ends_at`，
/// 窗口判断优先用上游时间（见 [`claim_daily_checkin`]）。容器 `TZ=UTC` 时
/// 本地时钟到不了 19:00，再把本地常量当唯一判据就会把「活动已经开了」误判成
/// 「窗口还没开」（issue #32）。常量只在「一个套餐都没探到」时决定文案与
/// `retryAt`，以及上游没给 `starts_at` 时兜底。
///
/// 与 `core::auto_checkin::times` 的 `PROVIDER_TIMES` 是同一份事实的两处表达：
/// 那边是**调度**用的（决定几点跑），这里是**执行体**用的（兜底判断）。
/// 改一处就要改另一处。
const DAILY_WINDOW_HOUR: i64 = 19;

/// 今天的窗口开启时刻（Unix 毫秒时间戳，北京时间口径）。
fn daily_window_start(now: i64) -> i64 {
    let offset = i64::from(crate::beijing::OFFSET_SECONDS) * 1000;
    (now + offset).div_euclid(86_400_000) * 86_400_000
        + DAILY_WINDOW_HOUR * 3_600_000 - offset
}

/// 执行一次签到。`manual` 只影响日志措辞（本家没有写请求限流闸）。
///
/// 窗口判断优先用上游每期活动自带的 `starts_at`（`marketing/touch` 与
/// `billing/preview` 都会带），只有「一个套餐都没探到」时才回落到本地
/// [`DAILY_WINDOW_HOUR`] —— 容器 `TZ=UTC` 不影响这条判断（issue #32）。
///
/// 返回 `{success, msg, ...}` 形状与其余各家一致（`billing::checkin` 的
/// `claim_result` 只认 `success` / `msg`），另外带：
///   - `alreadyCompleted: true` —— 今天已领（落 `checkinAt` 用）；
///   - `outcome: "inactive"` —— 窗口没开 / 没有可领套餐，按 `retryAt` 稍后重试；
///   - `outcome: "captcha"` —— 需要人工完成验证，不把它误当成等待活动。
pub async fn claim_daily_checkin(
    ctx: &PluginCtx,
    region: Region,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let started = tokio::time::Instant::now();

    let record = crate::store::record(ctx, region.provider_id(), account_id)
        .await
        .map_err(GatewayError::into_message)?;
    let jwt = crate::store::pick(&record, &["jwt"]);
    let device_mid = crate::store::pick(&record, &["deviceMid", "device_mid"]);
    let user_id = crate::store::pick(&record, &["userId", "user_id"]);
    let proxy = record_proxy(&record);
    let mid = (!device_mid.is_empty()).then_some(device_mid.as_str());

    if jwt.trim().is_empty() {
        // 没有套餐 JWT 就没有领取资格：这条链服务的是「网页登录」建的账号，
        // 手工粘贴 API Key 的账号领不了（与 `missing_jwt` 那句同义，但这是
        // 无人值守路径，措辞要说明「这是账号形态问题，不是你漏配了什么」）。
        return Ok(failed_soft(format!(
            "该 ZCode {}账号没有套餐令牌（jwt），无法领取每日赠送额度；请用「网页登录」重新添加",
            region.label()
        )));
    }

    // ── 1. 补报激活事件（资格的前置动作）──────────────────────
    // 失败不阻断：领取有它自己的路径，且激活事件本来只是「把资格补上」的加试。
    if !user_id.trim().is_empty() {
        if let Err(error) = activation::report(region, &user_id, &device_mid, proxy.as_ref()).await
        {
            crate::logging::verbose(
                "[Checkin]",
                &format!("ZCode {}账号激活事件上报失败：{error}", region.label()),
            );
        }
    }

    claim::refresh_app_version_override(ctx).await;
    let snapshot = match claim::preview(
        ctx, region, account_id, &jwt, mid, &user_id, proxy.as_ref(), claim::PreviewPurpose::Automatic, false,
    ).await {
        Ok(snapshot) => snapshot,
        Err(error) => return Ok(failed_soft(error.message)),
    };
    // ── 2. 用上游 starts_at 判断「开始没有」，不再前置拦截 ──────
    // 以前在探测之前就用本地常量 19:00 拦住：容器 TZ=UTC 时本地时钟到不了
    // 19:00，营销来源也被一起短路（issue #32）。现在先探测，再按上游给的
    // 每期活动时间过滤；常量只在「一个套餐都没探到」时兜底。
    //
    // `ClaimablePlan.starts_at` 是**秒**（与 `ends_at` 同一口径），本地比较
    // 用毫秒，所以统一 `saturating_mul(1000)`。
    let now = crate::http::now_ms();
    let (plans, upcoming): (Vec<_>, Vec<_>) = snapshot
        .plans
        .into_iter()
        .partition(|plan| plan.starts_at.is_none_or(|start| start.saturating_mul(1000) <= now));
    if plans.is_empty() {
        if let Some(start) = upcoming.iter().filter_map(|plan| plan.starts_at).min() {
            // 有套餐、但全部还没到开始时间：用上游时间渲染，别再说「每天 19:00」。
            let at = start.saturating_mul(1000);
            let message = match crate::beijing::date_time_of_ms(at) {
                Some(time) => format!(
                    "今日赠送套餐的活动将于 {} 开始，稍后自动重试",
                    time.format("%m-%d %H:%M")
                ),
                None => {
                    "今日赠送套餐的活动窗口还没开（每天 19:00 起可领），稍后自动重试".to_string()
                }
            };
            return Ok(inactive(message, Some(at + 60_000)));
        }
        if snapshot.preview_error.is_some()
            || snapshot.marketing_error.is_some()
            || snapshot.ledger_error.is_some()
        {
            return Ok(failed_soft("部分套餐来源或领取台账查询失败，尚不能确认今日领取状态"));
        }
        // 一个套餐都没探到：本地常量兜底（上游没给 starts_at / 活动还没上架）。
        let window = daily_window_start(now);
        if now < window {
            return Ok(inactive(
                "今日赠送套餐的活动窗口还没开（每天 19:00 起可领），稍后自动重试",
                Some(window + 60_000),
            ));
        }
        return Ok(inactive("当前没有可领取的赠送套餐", None));
    }
    let expected = plans.len();
    let mut results = Vec::new();
    for plan in &plans {
        if ROUND_TIMEOUT.saturating_sub(started.elapsed()) < std::time::Duration::from_secs(45) {
            results.push(inactive("本轮领取时间已用完，剩余套餐稍后继续", Some(crate::http::now_ms() + 600_000)));
            break;
        }
        let attempt = claim::claim_plan(
            ctx, region, account_id, &jwt, mid, &user_id, proxy.as_ref(), plan, None,
            Some(started + ROUND_TIMEOUT),
        ).await;
        let stop = match &attempt {
            Ok(attempt) => matches!(&attempt.outcome,
                claim::ClaimOutcome::Failed { failure: claim::ClaimFailure::Captcha | claim::ClaimFailure::LoginRequired, .. }
                | claim::ClaimOutcome::Failed { code: 3012, .. }
            ),
            Err(_) => true,
        };
        results.push(plan_result(region, plan, attempt));
        if stop { break; }
    }
    Ok(combine_results(results, expected, snapshot.daily_checkin))
}

fn plan_result(
    region: Region,
    plan: &claim::ClaimablePlan,
    attempt: Result<claim::ClaimAttempt, GatewayError>,
) -> Value {
    let plan_id = &plan.plan_id;
    let plan_name = &plan.name;
    match attempt {
        Ok(attempt) => {
            let mut result = match attempt.outcome {
                claim::ClaimOutcome::Claimed { ends_at, .. } => {
                    let until = ends_at
                        .and_then(|at| crate::beijing::date_time_of_ms(at.saturating_mul(1000)))
                        .map(|time| time.format("%m-%d %H:%M").to_string());
                    let msg = match until {
                        Some(until) => format!("领到「{plan_name}」（{plan_id}），有效期至 {until}"),
                        None => format!("领到「{plan_name}」（{plan_id}）"),
                    };
                    crate::logging::log("[Checkin]", &format!("✅ ZCode {}账号签到成功：{plan_id}", region.label()));
                    json!({ "success": true, "msg": msg, "planId": plan_id, "planName": plan_name, "claimedAt": attempt.claimed_at })
                }
                claim::ClaimOutcome::Failed { failure, code, message, failure_ends_at, .. } => {
                    use claim::ClaimFailure;
                    match failure {
                        ClaimFailure::AlreadyClaimed => json!({
                            "success": false, "alreadyCompleted": true,
                            "msg": format!("「{plan_name}」已领取"), "planId": plan_id,
                            "claimedAt": attempt.claimed_at,
                        }),
                        ClaimFailure::Unavailable | ClaimFailure::QuotaExhausted => inactive(
                            format!("「{plan_name}」{}（{code}）：{message}", failure.label()),
                            failure_ends_at.map(|at| at.saturating_mul(1000)),
                        ),
                        ClaimFailure::Captcha => json!({
                            "success": false, "outcome": "captcha",
                            "planId": plan_id, "planName": plan_name,
                            "msg": format!("「{plan_name}」需要人工验证：{message}"),
                        }),
                        other => failed_soft(format!("「{plan_name}」{}（{code}）：{message}", other.label())),
                    }
                }
            };
            result["marketingError"] = json!(attempt.marketing_error);
            result["dailyCheckin"] = attempt.daily_checkin;
            result
        }
        Err(error) => failed_soft(error.message),
    }
}

fn combine_results(results: Vec<Value>, expected: usize, daily_checkin: Value) -> Value {
    let daily_checkin = results.iter().rev().find_map(|row| row.get("dailyCheckin")).cloned()
        .unwrap_or(daily_checkin);
    let succeeded = results.iter().any(|row| row.get("success").and_then(Value::as_bool) == Some(true));
    let completed = daily_checkin.get("completed").and_then(Value::as_bool) == Some(true)
        && !results.is_empty() && results.len() == expected
        && results.iter().all(|row| row.get("success").and_then(Value::as_bool) == Some(true)
            || row.get("alreadyCompleted").and_then(Value::as_bool) == Some(true));
    let needs_verification = results.iter().any(|row| row.get("outcome").and_then(Value::as_str) == Some("captcha"));
    let pending = results.iter().any(|row| row.get("outcome").and_then(Value::as_str) == Some("inactive"));
    let failed = results.iter().any(|row| row.get("success").and_then(Value::as_bool) != Some(true)
        && row.get("alreadyCompleted").and_then(Value::as_bool) != Some(true)
        && !matches!(row.get("outcome").and_then(Value::as_str), Some("inactive" | "captcha")));
    let mut msg = results.iter().filter_map(|row| row.get("msg").and_then(Value::as_str))
        .collect::<Vec<_>>().join("；");
    if !completed && daily_checkin.get("sourcesComplete").and_then(Value::as_bool) != Some(true) {
        msg.push_str("；部分套餐来源查询失败，已处理已知套餐，今日领取状态仍待确认");
    }
    let mut result = json!({
        "success": succeeded && completed,
        "alreadyCompleted": !succeeded && completed,
        "msg": msg,
        "outcome": if completed { if succeeded { "claimed" } else { "done" } }
            else if failed { "failed" } else if needs_verification { "captcha" }
            else if pending { "inactive" } else { "failed" },
        "results": results,
        "dailyCheckin": daily_checkin,
    });
    let retry_at = result["results"].as_array().and_then(|rows| rows.iter()
        .filter_map(|row| row.get("retryAt").and_then(Value::as_i64)).min());
    if let Some(at) = retry_at { result["retryAt"] = json!(at); }
    result
}

/// 中性未领取（不落 `checkinAt`，按 `retryAt` 稍后重试）。
fn inactive(message: impl Into<String>, retry_at: Option<i64>) -> Value {
    let mut value = json!({
        "success": false,
        "outcome": "inactive",
        "msg": message.into(),
    });
    if let Some(at) = retry_at {
        if let Some(object) = value.as_object_mut() {
            // 网关的重试链只认毫秒；上游给的是秒（`failure_ends_at`），调用方已换算
            object.insert("retryAt".to_string(), Value::from(at.max(crate::http::now_ms() + 600_000)));
        }
    }
    value
}

/// 失败（进 `failed` 台账）。措辞以「读得懂」为准，不暴露协议细节。
fn failed_soft(message: impl Into<String>) -> Value {
    json!({ "success": false, "msg": message.into() })
}

/// 账号记录上的出网代理（与 `mod.rs` 的 `account_proxy` 同一取值口径；
/// 那个函数是私有的，这里读同一格）。
fn record_proxy(record: &Value) -> Option<agent2api_provider_protocol::WireProxy> {
    record
        .get("proxy")
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}
