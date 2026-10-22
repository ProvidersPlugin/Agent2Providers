//! OfficeAce 的「新手任务 / 奖励活动」：把订阅里的 `bonus_skus` 当**已到账**的
//! 奖励账本列出来。
//!
//! ── 关键事实：账本条目 = 已发放（2026-10-13 实测确认）───────
//! `bonus_skus` 的条目是**发放凭证**：出现即已到账（官方 `AK()` 把所有未过期
//! 条目的 `points` 计入 `totalCredits`；实测三项 6000 分全在 `available` 里，
//! 6499.33 = 500 套餐 + 6000 奖励 − 0.668 已用）。所以「任务」一律显示为已领取，
//! `unclaimed` 恒 0 —— 未发放的那一天，条目**还没出现在账本里**。
//!
//! ── 领取动作做什么（`claim_all` 与签到是同一个上游调用）─────
//! `POST /v1/subscription/bonus/claim` 是「**立即触发今天的结算**」：上游按天
//! 结算一批（官方客户端每小时自动打这一发），已在官方客户端结算过时返回空列表
//! `{"bonus_skus": []}`。所以 claim 之后要**重查一次订阅**才是展示用的全量账本。
//! 响应形状对齐 `codearts::onboarding`（签到中心前端读那套）。
//!
//! ── `settled` 为什么还要本地台账 ─────────────────────────────
//! 上游不直接回答「今天经本网关结算过没有」。台账（账号记录 `bonusClaims`）的
//! `all` 位就是这个事实：置位后当天不再自动补打 claim（见 [`Ledger`]）。日界
//! 口径复用 `codearts::welfare::today`（北京自然日）——上游活动周期就是 UTC+8
//! 零点，两处同源才不会有「今天」的分歧。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::collections::BTreeSet;

use serde_json::{json, Value};

use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;
use crate::logging;

use super::{credentials, subscription};

/// 任务分组名（签到中心按它分块显示）。
const TASK_GROUP: &str = "奖励积分";

/// 本地领取台账：`{ "day": "YYYY-MM-DD", "items": ["<activityId>"…], "all": bool }`。
///
/// ── 它现在管什么（2026-10-13 收窄，别再拿它判 done）────────────
/// `done` 的判据已改为「账本条目 = 已发放 = 已领取」（见 [`build_tasks`]），
/// 台账只剩 `all` 有用：它回答「**今天经本网关结算过没有**」——`get_tasks`
/// 的 `settled` 位与「进页面是否还自动打一发 claim」都读它。
/// `claimed`（今天领过哪些 key）保留为记录：`mark_claimed` 的语义要它完整，
/// 上游哪天按 key 判断也有据可查。
struct Ledger {
    day: String,
    claimed: BTreeSet<String>,
    /// 今天是否**整体领过一次**（签到或「一键领取」成功即置位）。
    all: bool,
}

/// 台账的 ledger 键名（`store.ledger_get` / `ledger_put` 的 `kind`）。
pub const LEDGER_KIND: &str = "officeaceBonus";

/// 读台账；跨天（或没台账 / 记录坏了）都当空表 —— 只回答「**今天**领过没有」。
async fn ledger_of(ctx: &PluginCtx, account_id: &str) -> Ledger {
    let day = crate::beijing::today_key();
    let stored = ctx
        .ledger_get(account_id, LEDGER_KIND)
        .await
        .ok()
        .filter(|stored| stored.get("day").and_then(Value::as_str) == Some(day.as_str()));
    let Some(stored) = stored else {
        return Ledger { day, claimed: BTreeSet::new(), all: false };
    };
    let claimed = stored
        .get("items")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let all = stored.get("all").and_then(Value::as_bool).unwrap_or(false);
    Ledger { day, claimed, all }
}

async fn persist_ledger(ctx: &PluginCtx, account_id: &str, ledger: &Ledger) {
    let payload = json!({
        "day": ledger.day,
        "items": ledger.claimed.iter().cloned().collect::<Vec<_>>(),
        "all": ledger.all,
    });
    if let Err(error) = ctx.ledger_put(account_id, LEDGER_KIND, &payload).await {
        // 落盘失败不改签到结果：上游那边可能已经发下来了，把成功报成失败本末倒置。
        logging::verbose(
            "[OfficeAce]",
            &format!("账号 {account_id} 的奖励台账未能落盘：{error}"),
        );
    }
}

/// 把一批活动记进「今天已处理」台账（签到与领取共用），并置「整体领过」位。
///
/// ── 为什么签到也要调它 ──────────────────────────────────────
/// OfficeAce 的签到与新手任务是**同一个上游动作**（一次 claim 就把当天该发的
/// 都发了）。台账的 `all` 位 = 「今天经本网关结算过」：置位后签到中心不再为这个
/// 账号自动补打 claim（`settled` 位，见 [`Ledger`]），当天也就到此为止。
pub async fn mark_claimed(ctx: &PluginCtx, account_id: &str, bonuses: &[subscription::Bonus]) {
    let mut ledger = ledger_of(ctx, account_id).await;
    // 即便这次一个活动都没列出来（上游当时没给 bonus_skus），也置位：
    // 这次 claim 本身已经把当天能领的都领了。
    ledger.all = true;
    for bonus in bonuses {
        ledger.claimed.insert(bonus.activity_id.clone());
    }
    persist_ledger(ctx, account_id, &ledger).await;
}

/// 任务行 + 汇总：返回 `(tasks, total, earned, unclaimed)`。
///
/// ── `done` 的判据（2026-10-13 修正，别再改回去）───────────────
/// **上游账本里的条目本身就是发放凭证**：出现的都已到账。官方口径同此
/// （`AK()` 把所有未过期 `bonus_skus` 的 `points` 计入 `totalCredits`）。
/// 实测三项（新用户 4000 / 每日签到 1000 / 国庆 1000）全部计入
/// `available`（6499.33 = 500 套餐 + 6000 奖励 − 0.668 已用），而 claim
/// 返回 `{"bonus_skus": []}` —— 「列表里的都已发、没有新可领」。
///
/// 上一版拿本地台账判 done：官方客户端发出的奖励它不知道 ⇒ 已到账的显示成
/// 「可领取」，用户点 claim 又拿到空列表 ⇒ 「领了没反应」。台账现在只服务
/// `settled`（今天经本网关结算过），不参与 done。
fn build_tasks(bonuses: &[subscription::Bonus], _ledger: &Ledger) -> (Vec<Value>, f64, f64, usize) {
    let total: f64 = bonuses.iter().map(|bonus| bonus.total).sum();
    let tasks: Vec<Value> = bonuses
        .iter()
        .map(|bonus| {
            json!({
                "key": bonus.activity_id.clone(),
                "title": bonus.name.clone(),
                "group": TASK_GROUP,
                "points": bonus.total,
                // 账本条目 = 已发放 ⇒ 一律已领取（见函数文档）
                "done": true,
                "claimable": false,
                "remaining": bonus.remaining(),
                "expiresAt": bonus.expires_at,
            })
        })
        .collect();
    // unclaimed 恒 0：账本里没有「待领」的东西（待领的那一天，条目还不在账本里）
    (tasks, total, total, 0)
}

/// 取账号与其网关/控制面凭据（两处入口共用）。
async fn credential_of(
    ctx: &PluginCtx,
    account_id: &str,
) -> Result<credentials::OfficeAceCredential, GatewayError> {
    let record = super::record_of(ctx, account_id).await?;
    credentials::from_record(Some(&record))
}

/// 任务状态（只读）：列订阅里的奖励活动。
///
/// ── `refresh` 在本家为什么**不改变取数**────────────────────
/// 别家（Loomy / CodeArts / 小浣熊）的新人礼是**一次性**的，结清后写进
/// `onboarding_memory` 永久记忆，`refresh=false` 就直接吃记忆、零上游请求。
/// 本家不是那个形状：奖励**每天**由上游结算一批（官方客户端每小时自动打一发
/// claim），第二天上游会开新一批活动 —— 用「永久结清」那份记忆会把明天新开的
/// 活动也判成已领完。所以本家用**按天**的台账（[`ledger_of`]），它今天没结清
/// 就照样实查；而这里的读请求只是一次 `GET /v1/subscription`（无副作用、不领
/// 东西），真正会白打的那一发（claim）已经由台账在 [`claim_all`] 里挡住。
/// `settled` 字段给的是「**今天经本网关结算过**」（台账 all 位）——界面据此
/// 在领完后跳过自动查询。注意它**不能**用 `unclaimed == 0` 代替：账本条目
/// 一律已发放，那个数恒为 0（见 [`build_tasks`]）。
pub async fn get_tasks(
    ctx: &PluginCtx,
    account_id: &str,
    _refresh: bool,
) -> Result<Value, GatewayError> {
    let credential = credential_of(ctx, account_id).await?;
    let subscription = subscription::fetch_subscription(&credential).await?;
    let bonuses = subscription::bonuses_of(&subscription);
    let ledger = ledger_of(ctx, account_id).await;
    let (tasks, total, earned, unclaimed) = build_tasks(&bonuses, &ledger);
    Ok(json!({
        "tasks": tasks,
        "earned": earned,
        "total": total,
        "unclaimed": unclaimed,
        // 今天经本网关结算过 ⇒ 界面可跳过自动查询（次日台账按天复位）
        "settled": ledger.all,
        "provider": "officeace",
        "note": "OfficeAce 的奖励由上游按天自动发放（官方客户端每小时打一发 claim）；这里列出的是**已到账**的奖励账本，「一键领取」= 立即触发今天的结算",
    }))
}

/// 领取全部未领的奖励（与每日签到同一个上游动作，见模块头）。
///
/// ── 为什么 claim 之后要再查一次订阅（2026-10-13 修）──────────
/// claim 的响应只带**本次新发放的**条目（实测：今天已结算时是空列表
/// `{"bonus_skus": []}`）。上一版直接拿它当展示账本 ⇒ 已发放的奖励全从界面
/// 消失、前端只好「为空就保留旧数据」⇒ 用户看到的是「点了没反应」。
/// 现在：claim 成功后**重查一次** `GET /v1/subscription` 拿全量账本（查不到
/// 就退 claim 前那份），并返回一句 `message` 交代本次到底有没有新到账。
pub async fn claim_all(ctx: &PluginCtx, account_id: &str) -> Result<Value, GatewayError> {
    let credential = credential_of(ctx, account_id).await?;
    let before = subscription::fetch_subscription(&credential).await?;
    // claim 本身失败要如实抛错（凭据失效 / 上游拒绝）—— 那与「没有新可领」不是一回事
    let newly = subscription::bonuses_of(&subscription::claim_bonus(&credential).await?);
    let snapshot = match subscription::fetch_subscription(&credential).await {
        Ok(snapshot) => snapshot,
        Err(_) => before.clone(),
    };
    let previous = subscription::remaining_map(&before);
    let bonuses = subscription::bonuses_of(&snapshot);
    let mut ledger = ledger_of(ctx, account_id).await;
    let mut results = Vec::new();
    let mut claimed = 0usize;
    let mut claimed_points = 0.0f64;
    for bonus in &bonuses {
        let key = bonus.activity_id.clone();
        let title = bonus.name.clone();
        let remaining = bonus.remaining();
        // 增量口径与签到一致：对得上 claim 前读数的看差值；claim 后新出现的
        // 条目（`previous` 里没有）整笔都算本次到账
        let gained = match previous.get(&key) {
            Some(prev) => (remaining - prev).max(0.0),
            None => remaining,
        };
        let already = gained <= 0.0;
        if !already {
            claimed += 1;
            claimed_points += gained;
        }
        ledger.claimed.insert(key.clone());
        results.push(json!({
            "key": key,
            "title": title,
            "ok": true,
            "already": already,
            "gained": gained,
        }));
    }
    // 一次 claim 把整个集合都结算了：置「今天整体领过」位（settled 用，见 build_tasks）
    ledger.all = true;
    persist_ledger(ctx, account_id, &ledger).await;
    let (tasks, total, earned, unclaimed) = build_tasks(&bonuses, &ledger);
    let message = if claimed > 0 {
        format!("本次到账 {claimed_points:.0} 积分")
    } else if newly.is_empty() {
        // claim 返回空 = 今天已由上游结算过（官方客户端每小时会自动打这一发）
        "今天的奖励已发放，无需重复领取".to_string()
    } else {
        "奖励已在账上".to_string()
    };
    Ok(json!({
        "results": results,
        "claimed": claimed,
        "failed": 0,
        "claimedPoints": claimed_points,
        "tasks": tasks,
        "earned": earned,
        "total": total,
        "unclaimed": unclaimed,
        "settled": true,
        "message": message,
        "provider": "officeace",
    }))
}
