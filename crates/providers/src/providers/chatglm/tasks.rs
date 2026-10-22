//! 智谱清言任务中心（「做任务，赚积分」）：新手任务与每日任务。
//!
//! ── 上游契约（依据：`agentmore` 线上包的 Pinia store + 2026-10 实测）──
//! ```text
//!   GET  /chatglm/agent-api/task/center      分类 + 任务表 + user_info
//!   POST /chatglm/agent-api/task/click       {task_id} → {jump:{target,params}}
//!   POST /chatglm/agent-api/task/complete    {task_id} → {reward:{points_awarded}}
//!   GET  /chatglm/agent-api/task/rule_desc   规则正文（Markdown）
//! ```
//!
//! ── 三档分类（`categories[].code`）────────────────────────────
//! ```text
//!   newbie     新手任务   一次性，`progress{done,total}` 形如 0/5
//!   daily      每日任务   每天 00:00 (UTC+8) 重置，形如 0/2
//!   timeLimit  限时任务   目前只有「邀请新用户」（需分享口令给真人，无法代做）
//! ```
//!
//! ── 完成流程是**两步**，顺序不能反 ───────────────────────────
//! 官方客户端：`click`（登记任务会话、拿到跳转目标）→ 用户真去做那个动作 →
//! `complete`（结算，返回 `points_awarded`）。
//!
//! 实测两条硬规则：
//!   1. **没 click 直接 complete** → HTTP 200 + `status:500 internal server
//!      error`（不是 400，别被状态码骗了）；
//!   2. **重复 complete** → `points_awarded:0`，积分不变（幂等）。
//!
//! 上游**不校验动作是否真的发生**：click 之后立刻 complete，实测真到账
//! （`points_balance` 18644.05 → 19144.05）。这是上游的设计选择，不是本实现
//! 的绕过 —— 官方客户端也只是本地判定动作完成，没有把行为证据发给服务端。
//! 因此本实现「领取」= 对该账号所有未完成任务逐条 click+complete。
//!
//! ── 与仓库两个既有机制怎么接 ─────────────────────────────────
//! 用户要求：新手任务接我们的新手任务，每日任务接我们的每日签到。
//!
//! ```text
//!   新手任务(newbie)  → api::onboarding 的 chatglm 分支（get_tasks / claim_all）
//!                       一次性福利：全部 done 后落 `onboardingSettled` 记忆，
//!                       之后零上游请求（与 Loomy / 小浣熊 / CodeArts 同口径）。
//!   每日任务(daily)   → billing::checkin 的 chatglm 分支（claim_daily_tasks）
//!                       每天一次，与「每日登录积分」并列在同一次签到里执行。
//! ```
//!
//! ── 限时任务为什么不领 ──────────────────────────────────────
//! `timeLimit` 目前只有「邀请新用户赚积分」：要分享口令、好友真注册才到账，
//! 上游按 `daily_quota` 限次且需要真实用户行为。记为完成既无意义（不会到账）
//! 也超出「代领已有奖励」的范围，因此只读展示，不参与领取。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;
use crate::logging;

use super::balance::score_from_raw;
use super::{credentials, endpoints, http};

/// 上游任务状态码（与客户端 `Bw` 枚举逐字对齐）：1 未开始 / 2 已点击 / 3 已完成。
///
/// 只有 3 才算「完成」—— 2 表示用户点过「去完成」但还没真做完，仍要结算。
const STATUS_COMPLETED: i64 = 3;

/// 参与新手任务分类的上游 code
pub const CATEGORY_NEWBIE: &str = "newbie";
/// 参与每日任务分类的上游 code
pub const CATEGORY_DAILY: &str = "daily";

/// 这个账号是不是访客（访客没有任务中心，实测 401）。
///
/// 判定复用凭证模块（`credentials::from_record`：先看记录里的 `isGuest`，
/// 再看 JWT 声明），**读的是原始记录** —— 与公开形态那个 `isGuest`
/// （`chatglm_accounts::guest_of_record`，签到范围判定读的那份）同源，只是
/// 兜底方向相反：本函数**读不出来就当访客**（列进新手任务分组只会换来一次
/// 注定失败的查询），签到范围那边**读不出来就不排除**（凭证损坏的账号要留在
/// 清单里把故障如实报出来，不能跟着访客一起被静默踢掉）。
#[allow(dead_code)] // 访客排除由网关的签到中心按公开形态 `isGuest` 判定
pub async fn is_guest_account(ctx: &PluginCtx, account_id: &str) -> bool {
    let record = match crate::store::record(ctx, super::ID, account_id).await {
        Ok(record) => Some(record),
        Err(_) => None,
    };
    match credentials::from_record(record.as_ref()) {
        Ok(credentials) => credentials.is_guest,
        Err(_) => true,
    }
}

/// 取登录账号凭证；访客返回 `None`（访客没有任务中心，实测 401）。
async fn login_credentials(
    ctx: &PluginCtx,
    account_id: &str,
) -> Result<Option<credentials::ChatglmCredentials>, GatewayError> {
    let record = match crate::store::record(ctx, super::ID, account_id).await {
        Ok(record) => Some(record),
        Err(_) if account_id.is_empty() => None,
        Err(error) => return Err(error),
    };
    let credentials = credentials::from_record(record.as_ref())?;
    if credentials.is_guest {
        return Ok(None);
    }
    Ok(Some(credentials))
}

/// 拉一次任务中心原始响应（未拆封的 `result` 对象）。
///
/// 这是本模块唯一的上游读入口：新手任务与每日任务都从这一份里切分类，
/// 免得两条链路各打一次同一个接口。
pub async fn fetch_center(
    ctx: &PluginCtx,
    account_id: &str,
) -> Result<Option<Value>, GatewayError> {
    let Some(credentials) = login_credentials(ctx, account_id).await? else {
        return Ok(None);
    };
    let payload = http::get_json(
        endpoints::PATH_TASK_CENTER,
        &credentials.bearer(),
        &credentials.device_id,
        None,
        "任务中心查询",
    )
    .await?;
    let result = http::check_envelope(&payload, "任务中心查询")?;
    Ok(Some(result))
}

/// 从 `center` 结果里切出某一档分类（找不到给 `None`）。
pub fn category_of(center: &Value, code: &str) -> Option<Value> {
    center
        .get("categories")
        .and_then(Value::as_array)
        .and_then(|categories| {
            categories
                .iter()
                .find(|category| category.get("code").and_then(Value::as_str) == Some(code))
                .cloned()
        })
}

/// 分类里的任务数组（形状不对给空数组）。
fn tasks_of(category: &Value) -> Vec<Value> {
    category
        .get("tasks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn task_id_of(task: &Value) -> String {
    task.get("task_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

fn title_of(task: &Value) -> String {
    task.get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// 任务状态（缺失按「未开始」处理：宁可多领一次，上游幂等保证不重复加分）
fn status_of(task: &Value) -> i64 {
    task.get("status")
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

fn is_done(task: &Value) -> bool {
    status_of(task) >= STATUS_COMPLETED
}

/// 任务奖励积分（上游按**整分**给，与账户余额的百分之一口径不同）。
///
/// 依据：`daily` 任务 `reward.points: 500` 结算后 `points_awarded: 500`、
/// `points_balance` 恰好 +500；而余额字段 `left_score` 是百分之一
/// （见 `balance::score_from_raw`）。两者**不要混用**换算。
fn reward_points(task: &Value) -> i64 {
    task.pointer("/reward/points")
        .and_then(Value::as_i64)
        .or_else(|| task.pointer("/reward/points").and_then(Value::as_f64).map(|v| v as i64))
        .filter(|points| *points > 0)
        .unwrap_or(0)
}

/// `center.user_info.points_balance` 是**已折算的字符串**（实测 `"18644.05"`）。
fn balance_of(center: &Value) -> f64 {
    let raw = center.pointer("/user_info/points_balance");
    match raw {
        Some(Value::String(text)) => text.trim().parse::<f64>().unwrap_or(0.0),
        Some(Value::Number(number)) => number.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

// ── 新手任务（接 api::onboarding）──────────────────────────────

/// 把 `newbie` 分类翻成 onboarding 的响应形状。
///
/// 与 Loomy / 小浣熊 / CodeArts 对齐：`{tasks:[{key,title,group,points,done}],
/// earned, total, unclaimed, settled}`。`key` 直接用上游 `task_id`（稳定，
/// 且能直接拿去 click/complete，不必再维护一张映射表）。
fn newbie_payload(category: &Value) -> Value {
    let tasks = tasks_of(category);
    let rows: Vec<Value> = tasks
        .iter()
        .map(|task| {
            json!({
                "key": task_id_of(task),
                "title": title_of(task),
                "group": category.get("name").and_then(Value::as_str).unwrap_or("新手任务"),
                "points": reward_points(task),
                "done": is_done(task),
            })
        })
        .collect();
    let earned: i64 = tasks
        .iter()
        .filter(|task| is_done(task))
        .map(reward_points)
        .sum();
    let total: i64 = tasks.iter().map(reward_points).sum();
    let unclaimed = rows
        .iter()
        .filter(|row| !row.get("done").and_then(Value::as_bool).unwrap_or(false))
        .count();
    json!({
        "tasks": rows,
        "earned": earned,
        "total": total,
        "unclaimed": unclaimed,
        // 全部完成 ⇒ 一次性福利结清（与另外三家的 `settled` 同义）。
        // 空档**不算**结清：否则上游暂时没下发任务会被永久记成「领完了」。
        "settled": unclaimed == 0 && !tasks.is_empty(),
    })
}

/// 新手任务状态查询（`GET /api/accounts/{id}/onboarding` 的 chatglm 执行体）。
///
/// `refresh=false` 且记忆里已结算 ⇒ **零上游请求**（与 Loomy 同口径）。
pub async fn get_tasks(
    ctx: &PluginCtx,
    account_id: &str,
    refresh: bool,
) -> Result<Value, GatewayError> {
    if account_id.is_empty() {
        return Err(GatewayError::with_status(400, "缺少账号 id"));
    }
    let previous = crate::onboarding::load(ctx, account_id).await;
    if !refresh {
        if let Some(snapshot) = previous.as_ref() {
            return Ok(crate::onboarding::serve(snapshot));
        }
    }
    let Some(center) = fetch_center(ctx, account_id).await? else {
        return Ok(guest_onboarding());
    };
    let category = category_of(&center, CATEGORY_NEWBIE).unwrap_or_else(|| {
        json!({ "code": CATEGORY_NEWBIE, "name": "新手任务", "tasks": [] })
    });
    let mut payload = newbie_payload(&category);
    if let Some(object) = payload.as_object_mut() {
        object.insert("balance".to_string(), json!(balance_of(&center)));
    }
    crate::onboarding::remember(ctx, account_id, "智谱清言", &mut payload, previous.as_ref())
        .await;
    Ok(payload)
}

/// 访客的任务中心中性回答（访客没有积分体系，不算失败）。
fn guest_onboarding() -> Value {
    json!({
        "tasks": [],
        "earned": 0,
        "total": 0,
        "unclaimed": 0,
        "settled": false,
        "note": "访客账号没有任务中心，仅登录账号可参与",
    })
}

/// 对一条任务执行 click（登记会话）。成功返回 `jump`（可能为 null）。
///
/// `jump.target` 只是给官方客户端跳转用的；本网关不需要跳转，但 **click 本身
/// 是 complete 的前置**（见模块头），所以不能省。
async fn click_task(
    credentials: &credentials::ChatglmCredentials,
    task_id: &str,
) -> Result<Value, GatewayError> {
    let payload = http::post_json(
        endpoints::PATH_TASK_CLICK,
        &credentials.bearer(),
        &credentials.device_id,
        &json!({ "task_id": task_id }),
        None,
        "任务登记",
    )
    .await?;
    let result = http::check_envelope(&payload, "任务登记")?;
    Ok(result.get("jump").cloned().unwrap_or(Value::Null))
}

/// 对一条任务执行 complete（结算）。返回本轮到账积分。
///
/// `points_awarded: 0` = 此前已结算（幂等），不是错误。
async fn complete_task(
    credentials: &credentials::ChatglmCredentials,
    task_id: &str,
) -> Result<i64, GatewayError> {
    let payload = http::post_json(
        endpoints::PATH_TASK_COMPLETE,
        &credentials.bearer(),
        &credentials.device_id,
        &json!({ "task_id": task_id }),
        None,
        "任务结算",
    )
    .await?;
    // 上游把「没 click 就 complete」报成 status:500（HTTP 仍是 200），
    // `check_envelope` 会带着文案抛 502 —— 调用方逐条记失败。
    let result = http::check_envelope(&payload, "任务结算")?;
    Ok(result
        .pointer("/reward/points_awarded")
        .and_then(Value::as_i64)
        .unwrap_or(0))
}

/// 一条任务「登记 + 结算」。
///
/// 已完成的直接跳过（不发任何请求）：重复 complete 虽然幂等，但没必要多打两次。
async fn settle_one(
    credentials: &credentials::ChatglmCredentials,
    task: &Value,
) -> Result<(bool, i64), GatewayError> {
    let task_id = task_id_of(task);
    if task_id.is_empty() {
        return Err(GatewayError::with_status(502, "上游任务缺少 task_id"));
    }
    if is_done(task) {
        return Ok((false, 0));
    }
    click_task(credentials, &task_id).await?;
    let points = complete_task(credentials, &task_id).await?;
    Ok((points > 0, points))
}

/// 新手任务一键领取（`POST /api/accounts/{id}/onboarding/claim` 的 chatglm 执行体）。
///
/// 与 Loomy 的 `claim_all` 同构：已完成的跳过、串行、逐条记结果、401 立即整体失败、
/// 全部结清后落 `onboardingSettled` 记忆。
pub async fn claim_all(ctx: &PluginCtx, account_id: &str) -> Result<Value, GatewayError> {
    if account_id.is_empty() {
        return Err(GatewayError::with_status(400, "缺少账号 id"));
    }
    // 已结算 ⇒ 零上游（一次性福利没有「再领一次」的可能）
    if let Some(snapshot) = crate::onboarding::load(ctx, account_id).await {
        let mut payload = crate::onboarding::serve(&snapshot);
        if let Some(object) = payload.as_object_mut() {
            object.insert("results".to_string(), json!([]));
            object.insert("claimed".to_string(), json!(0));
            object.insert("failed".to_string(), json!(0));
            object.insert("claimedPoints".to_string(), json!(0));
        }
        return Ok(payload);
    }
    let Some(credentials) = login_credentials(ctx, account_id).await? else {
        return Ok(json!({
            "results": [],
            "claimed": 0,
            "failed": 0,
            "claimedPoints": 0,
            "tasks": [],
            "earned": 0,
            "total": 0,
            "unclaimed": 0,
            "settled": false,
            "note": "访客账号没有任务中心，仅登录账号可参与",
        }));
    };
    let Some(center) = fetch_center(ctx, account_id).await? else {
        return Ok(guest_onboarding());
    };
    let category = category_of(&center, CATEGORY_NEWBIE).unwrap_or_else(|| {
        json!({ "code": CATEGORY_NEWBIE, "name": "新手任务", "tasks": [] })
    });

    let mut rows: Vec<Value> = Vec::new();
    let mut claimed = 0_i64;
    let mut failed = 0_i64;
    let mut claimed_points = 0_i64;

    for task in tasks_of(&category).iter() {
        if is_done(task) {
            continue;
        }
        match settle_one(&credentials, task).await {
            Ok((awarded, points)) => {
                claimed += 1;
                claimed_points += points;
                rows.push(json!({
                    "key": task_id_of(task),
                    "ok": true,
                    "already": !awarded,
                    "points": points,
                }));
                logging::log(
                    "[Onboarding]",
                    &format!(
                        "智谱清言账号 {account_id}：「{}」{}",
                        title_of(task),
                        if awarded {
                            format!("+{points} 积分")
                        } else {
                            "此前已完成，跳过重复加分".to_string()
                        }
                    ),
                );
            }
            Err(error) => {
                if error.status_code == 401 {
                    return Err(error);
                }
                failed += 1;
                logging::verbose(
                    "[Onboarding]",
                    &format!(
                        "智谱清言账号 {account_id}：「{}」领取失败: {}",
                        title_of(task),
                        error.message
                    ),
                );
                rows.push(json!({
                    "key": task_id_of(task),
                    "ok": false,
                    "error": error.message,
                }));
            }
        }
    }

    // 领完再查一次：拿「现在真的都完成了」这个事实落记忆（而不是拿本地推算）
    let after = fetch_center(ctx, account_id).await.ok().flatten();
    let after_category = after
        .as_ref()
        .and_then(|center| category_of(center, CATEGORY_NEWBIE))
        .unwrap_or(category);
    let mut payload = newbie_payload(&after_category);
    if let Some(object) = payload.as_object_mut() {
        object.insert("results".to_string(), json!(rows));
        object.insert("claimed".to_string(), json!(claimed));
        object.insert("failed".to_string(), json!(failed));
        object.insert("claimedPoints".to_string(), json!(claimed_points));
        if let Some(center) = after.as_ref() {
            object.insert("balance".to_string(), json!(balance_of(center)));
        }
    }
    crate::onboarding::remember(ctx, account_id, "智谱清言", &mut payload, None).await;
    Ok(payload)
}

// ── 每日任务（接 billing::checkin）─────────────────────────────

/// 领完全部「每日任务」，返回签到契约的 `{success, msg}`。
///
/// 与 `claim_daily_login_score` 并列在同一次签到里执行：登录积分是「签到」，
/// 每日任务是「签到后顺手领的那几笔日常」。访客中性跳过。
///
/// 幂等由上游保证（已完成的任务不再发请求；即使发了也 `points_awarded:0`）。
pub async fn claim_daily_tasks(
    ctx: &PluginCtx,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let Some(credentials) = login_credentials(ctx, account_id).await? else {
        // 与 checkin.rs 的 `guest_result` 同一口径（中性，不算失败）
        return Ok(json!({
            "success": false,
            "msg": "访客账号没有任务中心，跳过（仅登录账号可领取每日任务）",
        }));
    };
    let Some(center) = fetch_center(ctx, account_id).await? else {
        return Ok(json!({ "success": false, "msg": "任务中心不可用" }));
    };
    let Some(category) = category_of(&center, CATEGORY_DAILY) else {
        return Ok(json!({ "success": true, "msg": "当前没有每日任务" }));
    };
    let tasks = tasks_of(&category);
    if tasks.is_empty() {
        return Ok(json!({ "success": true, "msg": "当前没有每日任务" }));
    }

    let mut messages: Vec<String> = Vec::new();
    let mut gained_total = 0_i64;
    for task in tasks.iter() {
        if is_done(task) {
            continue;
        }
        match settle_one(&credentials, task).await {
            Ok((awarded, points)) => {
                if awarded {
                    gained_total += points;
                    messages.push(format!("{} +{points}", title_of(task)));
                } else {
                    messages.push(format!("{} 已完成", title_of(task)));
                }
            }
            Err(error) => {
                if error.status_code == 401 {
                    return Err(error);
                }
                messages.push(format!("{} 失败：{}", title_of(task), error.message));
            }
        }
    }

    let progress = category
        .get("progress")
        .cloned()
        .unwrap_or_else(|| json!({ "done": 0, "total": tasks.len() }));
    let done = progress.get("done").and_then(Value::as_i64).unwrap_or(0);
    let total = progress.get("total").and_then(Value::as_i64).unwrap_or(0);
    if messages.is_empty() {
        messages.push(format!("每日任务已全部完成（{done}/{total}）"));
    }
    let message = messages.join("；");
    if gained_total > 0 {
        logging::log(
            "[Checkin]",
            &format!("✅ 智谱清言账号 {account_id}: 每日任务 +{gained_total}（{message}）"),
        );
    }
    Ok(json!({
        "success": true,
        "msg": message,
        "gained": gained_total,
        "progress": { "done": done, "total": total },
    }))
}

/// 每日任务的只读快照（签到中心展示用；零副作用）。
///
/// 返回答约同 [`claim_daily_tasks`]，`done`/`total` 直接取上游分档进度。
#[allow(dead_code)] // 签到中心的「今日任务进度」暂无插件侧挂点（协议没有这一格）
pub async fn daily_snapshot(
    ctx: &PluginCtx,
    account_id: &str,
) -> Result<Option<Value>, GatewayError> {
    let Some(center) = fetch_center(ctx, account_id).await? else {
        return Ok(None);
    };
    let Some(category) = category_of(&center, CATEGORY_DAILY) else {
        return Ok(None);
    };
    let tasks = tasks_of(&category);
    let rows: Vec<Value> = tasks
        .iter()
        .map(|task| {
            json!({
                "key": task_id_of(task),
                "title": title_of(task),
                "points": reward_points(task),
                "done": is_done(task),
            })
        })
        .collect();
    Ok(Some(json!({
        "tasks": rows,
        "progress": category.get("progress").cloned().unwrap_or(Value::Null),
        "balance": balance_of(&center),
    })))
}

/// `score_from_raw` 的再导出：任务奖励是**整分**，与余额的百分之一口径不同，
/// 这里显式区分开，免得后来者顺手把 /100 套到任务奖励上。
#[allow(dead_code)] // 口径说明的载体（任务奖励是整分，不做 /100）
pub fn raw_balance_hint() -> f64 {
    score_from_raw(100.0)
}
