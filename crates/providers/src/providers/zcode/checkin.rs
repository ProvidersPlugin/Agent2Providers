//! ZCode 的**每日签到** —— 就是「领当天那份赠送套餐」。
//!
//! ── 为什么签到就是领取（链路依据）──────────────────────────
//! ZCode 没有独立的签到接口，但上游每天给账号下发一份**带日期段的新套餐**
//! （`zcode-v3-start-plan-<日期>` / `zcode-v3-start-plan-1024` 这类），用户在
//! 客户端里点一下就领 —— 那一下就是这一家的「签到」。判据与 `claim.rs` 模块头
//! 记的一致：同一期再领回 `1003 already claimed`，所以「今天领过没」等价于
//! 「今天这个 plan_id 领过没」，无需本地记账。
//!
//! 与 `claim.rs` 里那个**手动领取**的差别只有三点：
//!   1. 这是**无人值守**的调度（每天定时跑），因此不弹滑块、不向用户要验证码；
//!   2. 拿不到验证码令牌时给**中性结果**（`inactive`，等下一轮），不是失败 ——
//!      与 Qoder 国际版「缺 UMID 组件」同一处置口径；
//!   3. 结果翻成 `CheckinReport`（`claimed` / `done` / `inactive` / `failed`），
//!      网关据此落 `checkinAt` 与台账。
//!
//! ── 人机验证：本家的死结与它现在的处置 ─────────────────────
//! `POST /api/v1/zcode-plan/billing/claim` 要求带
//! `X-Aliyun-Captcha-Verify-Param`（阿里云无痕验证），没有就是
//! `3007 captcha verify failed`。令牌只能由 WebView 里的阿里云 SDK 铸出来
//! （见 `captcha.rs` 模块头），Docker / headless 部署铸不出 —— 因此自动签到
//! 在那种部署上**必然拿不到令牌**。
//!
//! 处置：令牌池里有就用（桌面端开着界面时会自动补货，那是最常见的部署形态）；
//! 没有就走一次**不带令牌的领取尝试** —— 上游的风控配置可能是
//! `skip_model_request: true`（那一刻不要码），那时这一下就成功了。仍然 3007
//! 就回中性结果，提示「打开界面让它补铸令牌」，第二天照常再试。绝不自己造令牌。
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
/// （2026-10 的 ZCode 每日 3 亿 GLM-5.3-Flash 活动）。窗口没开时探测拿到的是
/// 空列表（`code: 0` + `plans: []`），不是错误 —— 所以必须靠本地时刻拦住，
/// 否则会像 Qoder 那样一路重试。
///
/// 与 `core::auto_checkin::times` 的 `PROVIDER_TIMES` 是同一份事实的两处表达：
/// 那边是**调度**用的（决定几点跑），这里是**执行体**用的（判断窗口开没开）。
/// 改一处就要改另一处。
const DAILY_WINDOW_HOUR: i64 = 19;

/// 今天的窗口开启时刻（毫秒时间戳，北京时间口径）。
///
/// 用 `now + 8h` 再取整日的方式算「北京时间今天零点」，与 Qoder 的
/// `daily_window_start` 同一算法（避免引入时区库依赖）。
fn daily_window_start() -> i64 {
    let beijing_now = crate::http::now_ms() + 8 * 3_600_000;
    beijing_now.div_euclid(86_400_000) * 86_400_000 + DAILY_WINDOW_HOUR * 3_600_000
}

/// 执行一次签到。`manual` 只影响日志措辞（本家没有写请求限流闸）。
///
/// 返回 `{success, msg, ...}` 形状与其余各家一致（`billing::checkin` 的
/// `claim_result` 只认 `success` / `msg`），另外带：
///   - `alreadyCompleted: true` —— 今天已领（落 `checkinAt` 用）；
///   - `outcome: "inactive"` —— 中性未领取（窗口没开 / 没有可领套餐 / 缺验证码），
///     网关不落账，按 `retryAt` 稍后重试。
pub async fn claim_daily_checkin(
    ctx: &PluginCtx,
    region: Region,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let now = crate::http::now_ms();
    let started = tokio::time::Instant::now();
    let window = daily_window_start();
    // 窗口没开：不探测、不领取，直接给中性结果并指定窗口开启时刻
    if now < window {
        return Ok(inactive(
            "今日赠送套餐的活动窗口还没开（每天 19:00 起可领），稍后自动重试",
            Some(window + 60_000),
        ));
    }

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
        ctx, region, account_id, &jwt, mid, &user_id, proxy.as_ref(), false,
    ).await {
        Ok(snapshot) => snapshot,
        Err(error) => return Ok(failed_soft(error.message)),
    };
    if snapshot.plans.is_empty() {
        if snapshot.preview_error.is_some() || snapshot.marketing_error.is_some() {
            return Ok(failed_soft("部分套餐来源查询失败，尚不能确认是否有可领取套餐"));
        }
        return Ok(inactive("当前没有可领取的赠送套餐", None));
    }
    let plans = snapshot.plans;
    let expected = plans.len();
    let mut results = Vec::new();
    for plan in &plans {
        if ROUND_TIMEOUT.saturating_sub(started.elapsed()) < std::time::Duration::from_secs(45) {
            results.push(inactive("本轮领取时间已用完，剩余套餐稍后继续", Some(crate::http::now_ms() + 600_000)));
            break;
        }
        let attempt = claim::claim_plan(
            ctx, region, account_id, &jwt, mid, proxy.as_ref(), plan, None,
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
    Ok(combine_results(results, expected))
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
                    json!({ "success": true, "msg": msg, "planId": plan_id, "planName": plan_name })
                }
                claim::ClaimOutcome::Failed { failure, code, message, failure_ends_at, .. } => {
                    use claim::ClaimFailure;
                    match failure {
                        ClaimFailure::AlreadyClaimed => json!({
                            "success": false, "alreadyCompleted": true,
                            "msg": format!("「{plan_name}」已领取"), "planId": plan_id,
                        }),
                        ClaimFailure::Unavailable | ClaimFailure::QuotaExhausted => inactive(
                            format!("「{plan_name}」{}（{code}）：{message}", failure.label()),
                            failure_ends_at.map(|at| at.saturating_mul(1000)),
                        ),
                        ClaimFailure::Captcha => inactive(
                            format!("「{plan_name}」领取需要有效的人机验证令牌，请在界面中主动完成验证后领取"),
                            None,
                        ),
                        other => failed_soft(format!("「{plan_name}」{}（{code}）：{message}", other.label())),
                    }
                }
            };
            result["marketingError"] = json!(attempt.marketing_error);
            result
        }
        Err(error) => failed_soft(error.message),
    }
}

fn combine_results(results: Vec<Value>, expected: usize) -> Value {
    let succeeded = results.iter().any(|row| row.get("success").and_then(Value::as_bool) == Some(true));
    let completed = !results.is_empty() && results.len() == expected
        && results.iter().all(|row| row.get("success").and_then(Value::as_bool) == Some(true)
            || row.get("alreadyCompleted").and_then(Value::as_bool) == Some(true));
    let pending = results.iter().any(|row| row.get("outcome").and_then(Value::as_str) == Some("inactive"));
    let failed = results.iter().any(|row| row.get("success").and_then(Value::as_bool) != Some(true)
        && row.get("alreadyCompleted").and_then(Value::as_bool) != Some(true)
        && row.get("outcome").and_then(Value::as_str) != Some("inactive"));
    let msg = results.iter().filter_map(|row| row.get("msg").and_then(Value::as_str))
        .collect::<Vec<_>>().join("；");
    let mut result = json!({
        "success": succeeded && completed,
        "alreadyCompleted": !succeeded && completed,
        "msg": msg,
        "outcome": if completed { if succeeded { "claimed" } else { "done" } }
            else if failed { "failed" } else if pending { "inactive" } else { "failed" },
        "results": results,
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
