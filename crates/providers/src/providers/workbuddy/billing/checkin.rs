//! WorkBuddy **国内版每日签到**（`POST /v2/billing/meter/daily-checkin`）。
//!
//! ── 与「每日活跃上报」的分工 ────────────────────────────────
//! 两套互不相通的连签（详见 `growth::daily_activity` 的模块头）：
//!
//! ```text
//! 签到活动   POST /v2/billing/meter/daily-checkin
//!            → checkin-activity-status 的 streak_days（签到连签，签到就涨）
//! 成长中心   POST /v2/report 的一条 chat_request_send 事件
//!            → activity/growth/streak 的 streak.days（连登 / 活跃地图）
//! ```
//!
//! 本文件只管前者；后者在 `admin` 的 `growth.daily` 动作里。
//!
//! ── 返回形状与网关逐字相同 ──────────────────────────────────
//! 网关的 `billing::normalize_daily_claim` 产两个形态：
//! ```text
//!   成功  { success: true, code: 0, data: <归一>, raw: <data> }
//!   失败  { success: false, code: <码>, msg: "...", requestId?: ... }
//! ```
//! 前端的签到结果卡按 `success` / `msg` 分流，所以形状不能改。
//!
//! ── 「今天已领过」为什么不算失败 ────────────────────────────
//! 上游对重复领取回业务错误，而那不是故障（每天只该领一次）。网关侧靠
//! `tolerate_duplicate_claim` 把那一档放行成 `Ok`；这里照同一口径：命中
//! 「已领」文案时返回 `success: false` 但**不报错**（`Err` 只表示请求没打成）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Map, Value};

use agent2api_provider_protocol as proto;

use crate::errors::GatewayError;
use crate::workbuddy_shared::endpoints::RESPONSE_CODE_OK;

use super::call;

/// 签到领取端点（国内版；带 `/v2` 前缀）。
const DAILY_CHECKIN_PATH: &str = "/v2/billing/meter/daily-checkin";

/// 领取一次国内版每日签到。
///
/// `session` 是 `session_of_record` 折出来的会话形状（`auth` / `account` /
/// `endpoint` / `proxy`）—— 与网关 `BillingService` 拿到的会话同一形态。
pub(crate) async fn claim(session: &Value) -> Result<Value, GatewayError> {
    let proxy = session
        .get("proxy")
        .and_then(|value| serde_json::from_value::<agent2api_provider_protocol::WireProxy>(value.clone()).ok());
    let result = call::call(
        "POST",
        DAILY_CHECKIN_PATH,
        None,
        session,
        proxy.as_ref(),
        None,
        // `expect_code_ok = false`：重复领取要读 msg 判「今天已领」；
        // `tolerate_duplicate_claim`：上游对重复领取回 HTTP 400（见 `call`）
        call::Options {
            expect_code_ok: false,
            tolerate_duplicate_claim: true,
            ..Default::default()
        },
    )
    .await;

    match result {
        Ok(result) => {
            if result.code == Some(RESPONSE_CODE_OK) && !result.data.is_null() {
                return Ok(json!({
                    "success": true,
                    "code": 0,
                    "data": normalize_checkin(&result.data),
                    "raw": result.data,
                }));
            }
            // 非 0 业务码：可能是「今天已领」，也可能是真的失败。
            // 两者都返回 `success:false` + msg —— 交给调用方的文案分流
            // （网关侧 `checkin_completed_today` 就是按 msg 子串判的）。
            let mut failure = Map::new();
            failure.insert("success".to_string(), Value::Bool(false));
            failure.insert(
                "code".to_string(),
                Value::from(result.code.unwrap_or(-1)),
            );
            failure.insert(
                "msg".to_string(),
                Value::String(result_msg(&result.raw).unwrap_or_else(|| "签到失败".to_string())),
            );
            Ok(Value::Object(failure))
        }
        // 请求本身没打成（超时 / 断连 / 401 / 非 2xx）—— 这是真的失败
        Err(error) => Err(error),
    }
}

/// 从响应体里取可读文案（`msg` 优先，空串当没有）。
fn result_msg(raw: &Option<Value>) -> Option<String> {
    let payload = raw.as_ref()?;
    payload
        .get("msg")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .or_else(|| {
            payload
                .get("message")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
        })
        .map(str::to_string)
}

/// 领取响应 → 统一签到形态（网关 `billing::request::normalize_checkin` 的移植）。
///
/// ── 为什么键名要摊平成驼峰 ──────────────────────────────────
/// 上游给的是蛇形（`checked_in` / `continuous_days`），而界面的签到卡读驼峰
/// （`checkedIn` / `continuousDays`）。两套名字并存的话，界面每次读都得写两份
/// 候选键 —— 归一次到这里，读侧就只有一套。
///
/// ── 取不到值的键为什么有时消失、有时是 null ─────────────────
/// 与 Node 的 `pick(...)` / `toInt(pick(...))` 语义逐字对应：
///   - `pick` 的返回值直接进对象，取不到就**不出这个键**（undefined 被
///     JSON.stringify 丢掉）；
///   - `toInt(pick(...))` 解不出时是 `null`，键**保留**。
/// 界面按「键在不在」区分「上游没说」与「上游说了 0」，所以不能统一。
pub(super) fn normalize_checkin(data: &Value) -> Value {
    if !data.is_object() {
        return data.clone();
    }
    let pick = |keys: &[&str]| -> Option<Value> {
        for key in keys {
            if let Some(value) = data.get(*key) {
                if !value.is_null() {
                    return Some(value.clone());
                }
            }
        }
        None
    };
    let int_of = |keys: &[&str]| -> Value {
        pick(keys)
            .and_then(|value| to_int(&value))
            .map(Value::from)
            .unwrap_or(Value::Null)
    };

    let mut result = Map::new();
    if let Some(value) = pick(&[
        "checked_in",
        "checkedIn",
        "is_checked_in",
        "isCheckedIn",
        "signed",
        "is_signed",
    ]) {
        result.insert("checkedIn".to_string(), value);
    }
    result.insert(
        "continuousDays".to_string(),
        int_of(&[
            "continuous_days",
            "continuousDays",
            "continuous_checkin_days",
            "streak",
            "serial_days",
        ]),
    );
    result.insert(
        "totalDays".to_string(),
        int_of(&["total_days", "totalDays", "total_checkin_days", "accumulate_days"]),
    );
    result.insert(
        "points".to_string(),
        int_of(&["points", "credit", "reward_points", "rewardPoints", "daily_points"]),
    );
    if let Some(value) = pick(&["start_time", "startTime"]) {
        result.insert("startTime".to_string(), value);
    }
    if let Some(value) = pick(&["end_time", "endTime"]) {
        result.insert("endTime".to_string(), value);
    }
    if let Some(value) = pick(&["activity_online_status", "activityOnlineStatus", "online"]) {
        result.insert("online".to_string(), value);
    }
    result.insert("raw".to_string(), data.clone());
    Value::Object(result)
}

/// JS `Number(x)` 的整数化（`to_int` 在网关侧的同义物）。
///
/// 认数字、布尔、以及字符串形式的数字；其余（对象 / 数组 / 非数字串）给 None。
fn to_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|float| float as i64)),
        Value::Bool(flag) => Some(i64::from(*flag)),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            trimmed
                .parse::<i64>()
                .ok()
                .or_else(|| trimmed.parse::<f64>().ok().map(|float| float as i64))
        }
        _ => None,
    }
}

/// claim 结果 → 协议层的 [`proto::CheckinReport`]。
///
/// ── 为什么判据是 `success` / `alreadyCompleted` / 文案三分 ─────
/// 上游把三件事混在同一个业务码空间里：
/// ```text
///   success: true          → 刚领到                      → Claimed
///   alreadyCompleted: true → 今天已经领过（不是失败）      → Done
///   msg 含「无此活动」等    → 这条通道没有这个签到         → Inactive
///   其余                    → 真失败                      → Failed
/// ```
/// `Done` 与 `Claimed` 在调度上是同一件事（今天不能再领），差别只在界面文案；
/// 把它们并成一个会让「今天已签」在台账里显示成「刚刚签到成功」。
pub(crate) fn report_of(claim: &Value) -> proto::CheckinReport {
    let success = claim
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let already = claim
        .get("alreadyCompleted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let message = claim
        .get("msg")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let outcome = if success {
        proto::CheckinOutcome::Claimed
    } else if already {
        proto::CheckinOutcome::Done
    } else if message
        .as_deref()
        .is_some_and(|text| text.contains("活动未开启") || text.contains("无此活动"))
    {
        // 「活动未开启」是正常状态：上游这期没开，不是账号有问题
        proto::CheckinOutcome::Inactive
    } else {
        proto::CheckinOutcome::Failed
    };
    // 连续天数：`data.continuousDays`（归一后的驼峰名）
    let streak = claim
        .get("data")
        .and_then(|data| data.get("continuousDays"))
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok());
    // 本次到账积分：`data.points`
    let credit = claim
        .get("data")
        .and_then(|data| data.get("points"))
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .map(|value| format!("+{value} 积分"));
    proto::CheckinReport {
        outcome,
        credit,
        streak,
        message,
    }
}
