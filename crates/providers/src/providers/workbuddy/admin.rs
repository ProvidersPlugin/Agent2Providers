//! WorkBuddy 的**管理动作**（协议方法 `admin.action` 的实现）。
//!
//! ── 这些动作原来是网关的 HTTP 端点 ───────────────────────────
//! ```text
//!   GET  /api/accounts/{id}/growth           → action = "growth.status"
//!   POST /api/accounts/{id}/growth/task      → action = "growth.task"
//!   POST /api/accounts/{id}/growth/school    → action = "growth.school"
//!   POST /api/accounts/{id}/growth/travel    → action = "growth.travel"
//!   POST /api/growth/travel/run              → action = "growth.travel.batch"
//!   （签到末尾的连登管家）                     → action = "growth.streak.batch"
//! ```
//! 迁移时**返回形状逐字保留**：前端读的就是这些键，形状一变界面就白屏。
//!
//! ── 为什么不用具名方法 ──────────────────────────────────────
//! 签到（`checkin`）是跨家有共识的语义，协议里有具名方法；这里是
//! WorkBuddy 独有的一整棵任务树 + 猫猫旅行 + 开学季，别家没有对应概念，
//! 塞进具名方法会让协议随长尾能力无限膨胀。理由见协议侧 `ADMIN_ACTION`。
//!
//! ── 批量动作为什么在插件里列账号 ────────────────────────────
//! 网关的定时任务原来是「读全量账号 → 传给 run_for_accounts」。迁移后
//! 网关不必把整批 secret 塞进 RPC：插件自己 `store.list` 本家账号即可 ——
//! 门控（`supports_growth` 只认国内版、企业号排除）本来就在插件里，
//! 账号间错峰的 800ms 也留在原处，时序与迁移前一致。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use agent2api_provider_sdk::PluginCtx;

use super::growth::{
    self,
    school, spec,
    travel::{self, AccountTravelReport},
    GrowthAccount, TaskOutcome,
};
use super::Region;
use crate::errors::GatewayError;

/// 本家支持的全部管理动作（网关只透传，取值域由这里定义）。
pub(super) const ACTIONS: &[&str] = &[
    "growth.status",
    "growth.task",
    "growth.school",
    "growth.travel",
    "growth.travel.batch",
    "growth.streak",
    "growth.streak.batch",
    "growth.daily",
    "activity.run",
    // ── 计费 / 运营（两家共用，见 `billing::ops`）─────────────────
    "billing.checkin-status",
    "billing.checkin-claim",
    "billing.reward",
    "billing.banner",
    "billing.ambassador",
];

/// 分发一次管理动作。
///
/// `account` 是网关点名的账号 id（批量动作不需要，传空串）；
/// `payload` 是厂商自定义参数（网关不理解内容）。
pub(super) async fn dispatch(
    ctx: &PluginCtx,
    provider_id: &str,
    account: &str,
    action: &str,
    payload: &Value,
) -> Result<Value, GatewayError> {
    let region = Region::from_provider_id(provider_id).unwrap_or(Region::Cn);
    // ── 计费 / 运营动作：两家共用，先分流 ────────────────────────
    // 它们与成长任务无关，且国际版也有其中几条（签到状态、banner、大使、
    // 每日活跃）。因此必须在下面那条「国际版只认 activity.run」的闸之前。
    if let Some(result) = billing_action(ctx, provider_id, account, action, payload).await? {
        return Ok(result);
    }
    // 国际版只剩一条动作：每日活跃（`activity.run`）。其余全是国内版成长域。
    if region == Region::Intl {
        if action != "activity.run" {
            return Err(GatewayError::with_status(
                400,
                format!("WorkBuddy 国际版只有「activity.run」这一个管理动作（收到「{action}」）"),
            ));
        }
        let record = crate::store::record(ctx, provider_id, account).await?;
        let mode = super::keepalive::Mode::parse(
            payload.get("mode").and_then(Value::as_str),
        );
        let models: Vec<String> = payload
            .get("models")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        return super::keepalive::run(&record, mode, &models).await;
    }
    if action == "activity.run" {
        return Err(GatewayError::with_status(
            400,
            "该账号不支持成长任务（仅 WorkBuddy 国内版）",
        ));
    }
    match action {
        "growth.status" | "growth.task" | "growth.school" | "growth.travel" => {
            let record = crate::store::record(ctx, provider_id, account).await?;
            let growth_account = growth_account_of(&record)?;
            match action {
                "growth.status" => growth_status(account, &growth_account).await,
                "growth.task" => growth_task(account, &growth_account, payload).await,
                "growth.school" => growth_school(account, &growth_account, payload).await,
                _ => growth_travel(account, &growth_account).await,
            }
        }
        "growth.travel.batch" => batch_travel(ctx, provider_id).await,
        // 单账号：网关批量入口里点名那一个（定时轮次扫全部时走 `.batch`）
        "growth.streak" => {
            let record = crate::store::record(ctx, provider_id, account).await?;
            Ok(streak_report(&[record], ctx).await)
        }
        // 批量：签到末尾的连登管家（网关不再传账号快照，插件自己列）
        "growth.streak.batch" => {
            let records = crate::store::list(ctx, provider_id).await?;
            Ok(streak_report(&records, ctx).await)
        }
        // 每日活跃上报（点亮成长中心连登 / 活跃地图）
        "growth.daily" => {
            let record = crate::store::record(ctx, provider_id, account).await?;
            let growth_account = growth_account_of(&record)?;
            Ok(daily_report(&growth_account).await)
        }
        other => Err(GatewayError::with_status(
            501,
            format!(
                "WorkBuddy 不认识管理动作「{other}」（可用：{}）",
                ACTIONS.join(" / ")
            ),
        )),
    }
}

/// 计费 / 运营动作的分流（见 [`ACTIONS`] 里那五条）。
///
/// `Ok(None)` = 不是计费动作，交给后面的成长任务分派。
async fn billing_action(
    ctx: &PluginCtx,
    provider_id: &str,
    account: &str,
    action: &str,
    payload: &Value,
) -> Result<Option<Value>, GatewayError> {
    if !action.starts_with("billing.") {
        return Ok(None);
    }
    let region = Region::from_provider_id(provider_id).unwrap_or(Region::Cn);
    let record = crate::store::record(ctx, provider_id, account).await?;
    if record.is_null() {
        return Err(GatewayError::with_status(401, "该账号没有可用凭证"));
    }
    let session = super::session_of_record(&record, region);
    let proxy: Option<agent2api_provider_protocol::WireProxy> = record
        .get("proxy")
        .and_then(|value| serde_json::from_value(value.clone()).ok());
    let value = match action {
        "billing.checkin-status" => {
            super::billing::ops::checkin_status(&session, region, proxy.as_ref()).await?
        }
        "billing.checkin-claim" => {
            super::billing::ops::claim_daily(&session, region, proxy.as_ref()).await?
        }
        "billing.reward" => {
            let kind = payload.get("kind").and_then(Value::as_str).unwrap_or("");
            super::billing::ops::claim_reward(&session, region, kind, proxy.as_ref()).await?
        }
        "billing.banner" => super::billing::ops::banner(&session, proxy.as_ref()).await,
        "billing.ambassador" => super::billing::ops::ambassador(&session, proxy.as_ref()).await,
        other => {
            return Err(GatewayError::with_status(
                400,
                format!("WorkBuddy 不认识计费动作「{other}」"),
            ))
        }
    };
    Ok(Some(value))
}

/// 跑连登管家并把报告摊成 JSON。
async fn streak_report(records: &[Value], ctx: &PluginCtx) -> Value {
    let reports = growth::streak::run_for_accounts(ctx, records).await;
    let done: usize = reports.iter().map(|report| report.done).sum();
    let failed: usize = reports.iter().map(|report| report.failed).sum();
    json!({
        "results": reports.iter().map(|report| json!({
            "accountId": report.account_id,
            "name": report.name,
            "done": report.done,
            "failed": report.failed,
            "messages": report.messages,
        })).collect::<Vec<Value>>(),
        "done": done,
        "failed": failed,
    })
}

/// 每日活跃上报的结果行（网关把它挂在签到结果行的 `growth` 键上）。
///
/// 形状与网关 `checkin_for` 里那段逐字一致：`{reported, streakDays, error}`。
/// **失败也是 200**（`error` 字段带原因）：这是一项附加收益，
/// 报成失败会让一次成功的签到看起来像出错了。
async fn daily_report(account: &GrowthAccount) -> Value {
    match growth::daily_activity::report(account).await {
        Ok(report) => json!({
            "reported": report.sent,
            "streakDays": report.streak_days.map(Value::from).unwrap_or(Value::Null),
            "error": Value::Null,
        }),
        Err(error) => json!({
            "reported": false,
            "streakDays": Value::Null,
            "error": error,
        }),
    }
}

/// 把扁平 secret 折成会话，再派生 growth 上下文。
fn growth_account_of(record: &Value) -> Result<GrowthAccount, GatewayError> {
    let session = super::session_of_record(record, Region::Cn);
    GrowthAccount::from_session(&session).ok_or_else(|| {
        GatewayError::with_status(400, "账号没有可用凭证，请先刷新登录态")
    })
}

/// 任务行的展示形状（界面按它渲染任务清单）。
fn task_row(task: &Value) -> Value {
    let code = task.get("task_code").and_then(Value::as_str).unwrap_or("");
    let spec = spec::spec_of(code);
    let progress = task.get("progress").cloned().unwrap_or(Value::Null);
    json!({
        "code": code,
        "title": task.get("title").and_then(Value::as_str).unwrap_or(""),
        "description": task.get("description").and_then(Value::as_str).unwrap_or(""),
        "status": task.get("accept_status").and_then(Value::as_str).unwrap_or(""),
        "current": progress.get("current").and_then(Value::as_i64).unwrap_or(0),
        "target": progress.get("target").and_then(Value::as_i64).unwrap_or(0),
        "locked": task.get("locked").and_then(Value::as_bool).unwrap_or(false),
        "unlockAt": task.get("unlock_at").cloned().unwrap_or(Value::Null),
        // 是否在映射表里（未映射任务在执行时会保守跳过 —— 界面如实标注）
        "supported": spec.is_some(),
        "unforgeable": spec.as_ref().map(|spec| spec.unforgeable).unwrap_or(false),
        "unforgeableReason": spec.as_ref().map(|spec| spec.reason).unwrap_or(""),
        "kind": spec.as_ref().map(|spec| spec.kind).unwrap_or(""),
        "source": spec.as_ref().map(|spec| spec.src).unwrap_or(""),
    })
}

/// `growth.status` —— 任务盘点（只读，不发任何写请求）。
///
/// 返回四块：成长任务清单、能量/连登读数、猫猫旅行状态、开学季概况。
/// 各块相互独立：某一块拉取失败只在该块给 `error`，不影响其余
/// （用户要看到「哪块坏了」）。
async fn growth_status(account_id: &str, account: &GrowthAccount) -> Result<Value, GatewayError> {
    // ── 成长任务清单 ──
    // 两个口径都要拉：小程序限定任务只在带 `X-Client-Platform: miniprogram`
    // 的列表里下发。合并时默认口径优先（同码不重复）。
    let tasks_block = match growth::list_tasks(account, false).await {
        Ok(default_tasks) => {
            let mut rows: Vec<Value> = default_tasks.iter().map(task_row).collect();
            let mut known: Vec<String> = rows
                .iter()
                .filter_map(|row| row.get("code").and_then(Value::as_str))
                .map(str::to_string)
                .collect();
            if let Ok(mp_tasks) = growth::list_tasks(account, true).await {
                for task in &mp_tasks {
                    let code = task.get("task_code").and_then(Value::as_str).unwrap_or("");
                    if code.is_empty() || known.iter().any(|item| item == code) {
                        continue;
                    }
                    known.push(code.to_string());
                    rows.push(task_row(task));
                }
            }
            json!({ "tasks": rows, "error": Value::Null })
        }
        Err(error) => json!({ "tasks": [], "error": error }),
    };

    // ── 能量 / 连登读数（只读 oracle，失败给 null）──
    let energy = growth::energy_balance(account).await;
    let streak = growth::growth_streak(account).await;

    // ── 猫猫旅行状态 ──
    let travel_block = match travel::buddy_info(account).await {
        Ok(buddy) => {
            let status = travel::travel_status(account).await;
            match (buddy, status) {
                (Some(buddy), Ok(status)) => json!({
                    "hasBuddy": true,
                    "buddyName": buddy.name,
                    "state": status.state,
                    "dailyLimitReached": status.daily_limit_reached,
                    "recordId": status.record_id,
                    "rewardCredit": status.reward_credit,
                    "error": Value::Null,
                }),
                (Some(buddy), Err(error)) => json!({
                    "hasBuddy": true,
                    "buddyName": buddy.name,
                    "state": "",
                    "error": error,
                }),
                (None, _) => json!({
                    "hasBuddy": false,
                    "buddyName": "",
                    "state": "",
                    "error": Value::Null,
                }),
            }
        }
        Err(error) => json!({ "hasBuddy": Value::Null, "error": error }),
    };

    // ── 开学季概况（任务列表 + 抽奖余额）──
    let school_block = match school::fetch_tasks(account).await {
        Ok((tasks, in_period)) => {
            let lottery = school::fetch_lottery(account).await;
            let chance = lottery
                .as_ref()
                .ok()
                .map(|(_, chance)| {
                    json!({
                        "balance": chance.get("balance").and_then(Value::as_i64).unwrap_or(0),
                        "totalEarned": chance.get("total_earned").and_then(Value::as_i64).unwrap_or(0),
                    })
                })
                .unwrap_or(Value::Null);
            json!({
                "inPeriod": in_period,
                "tasks": tasks.iter().map(|task| {
                    let code = task.get("task_code").and_then(Value::as_str).unwrap_or("");
                    json!({
                        "code": code,
                        "title": task.get("title").and_then(Value::as_str).unwrap_or(""),
                        "status": task.get("status").and_then(Value::as_str).unwrap_or(""),
                        "current": task.get("progress").and_then(Value::as_i64).unwrap_or(0),
                        "target": task.get("target_count").and_then(Value::as_i64).unwrap_or(0),
                        "rewardCredit": task.get("reward_credit").and_then(Value::as_i64).unwrap_or(0),
                    })
                }).collect::<Vec<Value>>(),
                "lottery": chance,
                "error": Value::Null,
            })
        }
        Err(error) => json!({ "inPeriod": false, "tasks": [], "lottery": Value::Null, "error": error }),
    };

    Ok(json!({
        "id": account_id,
        "uid": account.uid,
        "nick": account.nick,
        "energy": energy,
        "streak": streak,
        "tasks": tasks_block,
        "travel": travel_block,
        "school": school_block,
    }))
}

/// `growth.task` —— 执行**单个**任务（`payload = {code, mode}`）。
///
/// `mode=full`（缺省）：accept（若未接单）→ 点亮上报 → 回读 → claim；
/// `mode=claim`：只领奖（已完成/已领走幂等 claim，未完成跳过）。
async fn growth_task(
    account_id: &str,
    account: &GrowthAccount,
    payload: &Value,
) -> Result<Value, GatewayError> {
    let code = payload.get("code").and_then(Value::as_str).unwrap_or("");
    if code.is_empty() {
        return Err(GatewayError::with_status(400, "缺少任务码（code）"));
    }
    let mode = payload.get("mode").and_then(Value::as_str).unwrap_or("full");
    // 列表口径按任务的域走：小程序任务只在带 mp 头的列表里下发
    let mp = spec::spec_of(code).map(|spec| spec.mp).unwrap_or(false);
    let tasks = growth::list_tasks(account, mp)
        .await
        .map_err(|error| GatewayError::with_status(502, error))?;
    let Some(task) = growth::find_task(&tasks, code) else {
        return Ok(json!({
            "id": account_id,
            "code": code,
            "outcome": "skipped",
            "message": "任务不存在（可能已下线）",
            "credit": 0,
            "energy": 0,
        }));
    };
    let report = if mode == "claim" {
        // 只领奖：未完成的任务不动（服务端对未完成任务 claim 返回 not completed）
        let accept_status = task.get("accept_status").and_then(Value::as_str).unwrap_or("");
        let progress = task.get("progress").cloned().unwrap_or(Value::Null);
        let current = progress.get("current").and_then(Value::as_i64).unwrap_or(0);
        let target = progress.get("target").and_then(Value::as_i64).unwrap_or(1);
        if accept_status != "completed" && accept_status != "claimed" && current < target {
            growth::TaskReport {
                code: code.to_string(),
                outcome: TaskOutcome::Skipped,
                message: format!("未完成（{current}/{target}），未点亮不领取"),
                credit: 0,
                energy: 0,
            }
        } else {
            // 小程序限定任务的 claim 必须带 `X-Client-Platform: miniprogram`
            let (result, credit, energy, message) = growth::claim_task(account, code, mp).await;
            growth::TaskReport {
                code: code.to_string(),
                outcome: match result {
                    1 => TaskOutcome::Done,
                    2 => TaskOutcome::Already,
                    _ => TaskOutcome::Failed,
                },
                message,
                credit,
                energy,
            }
        }
    } else if code == "first_buddy" {
        growth::process_first_buddy(account, &task).await
    } else {
        growth::process_task(account, code, &task).await
    };
    Ok(json!({
        "id": account_id,
        "code": report.code,
        "outcome": report.outcome.as_str(),
        "message": report.message,
        "credit": report.credit,
        "energy": report.energy,
    }))
}

/// `growth.school` —— 开学季任务 + 可选抽奖（`payload = {only, lottery}`）。
async fn growth_school(
    account_id: &str,
    account: &GrowthAccount,
    payload: &Value,
) -> Result<Value, GatewayError> {
    let only: Vec<String> = payload
        .get("only")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let lottery = payload.get("lottery").and_then(Value::as_bool).unwrap_or(false);
    let (reports, lottery_result) = school::run_all(account, &only, lottery).await;
    let rows: Vec<Value> = reports
        .iter()
        .map(|report| {
            json!({
                "code": report.code,
                "outcome": report.outcome,
                "message": report.message,
            })
        })
        .collect();
    Ok(json!({
        "id": account_id,
        "results": rows,
        "lottery": lottery_result.unwrap_or(Value::Null),
    }))
}

/// `growth.travel` —— 猫猫旅行巡检一次（一趟只推进一步）。
async fn growth_travel(account_id: &str, account: &GrowthAccount) -> Result<Value, GatewayError> {
    let report = travel::run_once(account).await;
    Ok(json!({
        "id": account_id,
        "outcome": report.outcome,
        "message": report.message,
        "credit": report.credit,
    }))
}

/// `growth.travel.batch` —— 定时排程扫一遍本家全部账号。
///
/// 返回网关 `growth_scheduler::summarize` 的同形摘要（它直接落进配置的
/// `lastResult`，前端那格读的就是这几个键）。
async fn batch_travel(ctx: &PluginCtx, provider_id: &str) -> Result<Value, GatewayError> {
    let accounts = crate::store::list(ctx, provider_id).await?;
    let reports: Vec<AccountTravelReport> = travel::run_for_accounts(ctx, &accounts).await;
    let actions: Vec<Value> = reports
        .iter()
        .filter(|report| matches!(report.outcome, "adopt" | "depart" | "claim" | "failed"))
        .map(|report| {
            json!({
                "accountId": report.account_id,
                "name": report.name,
                "outcome": report.outcome,
                "message": report.message,
                "credit": report.credit,
            })
        })
        .collect();
    let credit: i64 = reports.iter().map(|report| report.credit).sum();
    let failed = reports
        .iter()
        .filter(|report| report.outcome == "failed")
        .count();
    Ok(json!({
        "at": crate::http::now_ms(),
        "accounts": reports.len(),
        "actions": actions,
        "credit": credit,
        "failed": failed,
    }))
}
