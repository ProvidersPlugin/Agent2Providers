//! 一次性新手任务的**本地结算记忆**（从网关 `providers::onboarding_memory` 搬来）。
//!
//! ── 与网关版的关键差别：存储位置 ────────────────────────────
//! 网关版把快照写进**账号记录的 `onboardingSettled` 字段**；插件拿不到记录字段的
//! 写权限（账号库在网关侧，只能读写 `secret`），所以改存 `ctx.ledger_put`
//! 的 `onboardingSettled` 台账。**形状与语义逐字不变** —— 网关那边读的是
//! `record.onboardingSettled`，插件这边读的是同名的 ledger kind，两边的
//! `snapshot` / `serve` / `remember` 因此可以共用同一套判据。
//!
//! ── 存什么 ──────────────────────────────────────────────────
//! `{ at, tasks: [...], earned, total }`，`tasks` 与状态查询返回的行同形
//! （结算那一刻全部 `done`）。语义只有一条：
//! **这个账号的新手任务（一次性福利）已经全部领完，之后不会再变。**
//!
//! ── 为什么要有它 ────────────────────────────────────────────
//! 新手任务是一次性福利：领完就没有了。没有这份记忆，每次进签到中心、每次
//! 签到后的自动补领都要为它重新打一次上游查询（Loomy 的任务表、CodeArts 的
//! 活动列表），而答案永远是「已领取」。
//!
//! ── 何时写 / 何时信 / 何时清 ────────────────────────────────
//!   · 写：[`remember`]，查询或领取拿到「没有待领」的事实那一刻。包括「别处
//!     （官方客户端）早就领过」的情形 —— 那同样说明这条一次性福利已经结清；
//!   · 信：[`load`] / [`serve`]，`?refresh=1` 之外的查询与全部领取；
//!   · 清：[`remember`] 收到 `settled` 为假时把旧快照抹掉 —— 上游出了新一期
//!     活动之类的新事实，记忆失效，回到「每次都查」的状态。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 本 crate 编译期禁止 unwrap/expect/panic（见 `main.rs` 的 deny）。

use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use crate::logging;

/// 结算记忆在台账里的 kind 名（与网关版的账号字段同名，便于人工对照）。
///
/// ⚠️ 这个名字是**落盘契约**：改了之后存量账号的记忆会读不到（表现为「已领完的
/// 账号又开始每次查上游」），所以不要改名。
pub const LEDGER_KIND: &str = "onboardingSettled";

/// 读结算快照；没有 / 形状读不懂（非对象）→ None。
pub async fn load(ctx: &PluginCtx, account_id: &str) -> Option<Value> {
    match ctx.ledger_get(account_id, LEDGER_KIND).await {
        Ok(value) => value.as_object().is_some().then_some(value),
        Err(_) => None,
    }
}

/// 用记忆回答一次状态查询：`tasks / earned / total` 来自快照，`unclaimed` 恒 0
/// （全部已领），另附 `settled` / `settledAt` 让界面能标出「这是记忆、不是刚
/// 查的」。调用方把自己那几项（provider / note）并进来。
pub fn serve(snapshot: &Value) -> Value {
    json!({
        "tasks": snapshot.get("tasks").cloned().unwrap_or(Value::Null),
        "earned": snapshot.get("earned").cloned().unwrap_or(Value::Null),
        "total": snapshot.get("total").cloned().unwrap_or(Value::Null),
        "unclaimed": 0,
        "settled": true,
        "settledAt": snapshot.get("at").cloned().unwrap_or(Value::Null),
    })
}

/// 按**响应自己的结论**落记忆，并把结果如实标回响应。
///
/// `settled == true`（这次的事实是「一次性福利全部到账」）⇒ 写入并返回快照本体，
/// 同时在响应里补 `settledAt`（结算时刻）。否则**清掉**旧快照并返回 None ——
/// 上游出了新事实（新一期活动、又出现待领项），记忆失效，回到「每次都查」。
///
/// `previous` 是**落库前**那份快照（调用方用 [`load`] 读，没有就传 `None`）。
/// 结论与旧快照一致（任务行逐字相同）时沿用旧的 `at`：`refresh=1` 手点查询会用
/// 同一份事实重新落一遍记忆，若每次刷新时间，「结算于」就变成了「刚才查过」，
/// 那不是它想说的意思（这条福利是什么时候结清的）。
///
/// 落库失败只记一条日志、仍返回快照 —— 上游那边福利已经结清，界面不该因为一次
/// 写盘失败就退回「还没查过」。
pub async fn remember(
    ctx: &PluginCtx,
    account_id: &str,
    label: &str,
    payload: &mut Value,
    previous: Option<&Value>,
) -> Option<Value> {
    let settled = payload.get("settled").and_then(Value::as_bool) == Some(true);
    if !settled {
        if ctx
            .ledger_put(account_id, LEDGER_KIND, &Value::Null)
            .await
            .is_err()
        {
            logging::verbose(
                "[Onboarding]",
                &format!("{label} 账号 {account_id}：结算记忆清除失败（不影响本次结果）"),
            );
        }
        return None;
    }
    let tasks = payload.get("tasks").cloned().unwrap_or(Value::Null);
    let at = match previous {
        // 同一份事实已经在记忆里 ⇒ 沿用原来的结算时刻
        Some(old) if old.get("tasks") == Some(&tasks) => {
            old.get("at").and_then(Value::as_i64).unwrap_or_else(|| crate::http::now_ms())
        }
        _ => crate::http::now_ms(),
    };
    let snapshot = json!({
        "at": at,
        "tasks": tasks,
        "earned": payload.get("earned").cloned().unwrap_or(Value::Null),
        "total": payload.get("total").cloned().unwrap_or(Value::Null),
    });
    if ctx
        .ledger_put(account_id, LEDGER_KIND, &snapshot)
        .await
        .is_err()
    {
        logging::log(
            "[Onboarding]",
            &format!(
                "⚠️ {label} 账号 {account_id}：新手任务结算记忆落库失败（本次结果不受影响，下次查询会重试）"
            ),
        );
    }
    if let Some(object) = payload.as_object_mut() {
        object.insert("settledAt".to_string(), json!(at));
    }
    Some(snapshot)
}
