//! Loomy 新手任务（onboarding）：状态查询与一键领取。
//!
//! ── 上游契约（依据：客户端 `electron/onboarding-service.js` + 实测）──
//! ```text
//!   GET  {集成网关}/api/v1/onboarding/tasks            header token: <session>
//!   POST {集成网关}/api/v1/onboarding/tasks/complete   body {"key": "<任务 key>"}
//! ```
//! 统一信封 `{code, desc, trace_id, data}`：`000000` 成功、`100002` 登录失效。
//!
//! ── 完成判定在客户端本地，上报不带行为证明 ──────────────────
//! 8 个任务的「完成」在 Loomy 客户端里全是本地事件判定（发消息、建定时任务、
//! 存远程控制配置、建搭子…），上报接口只带 `{key}`，没有签名或行为证据。
//! 实测（2026-10-08，production）：对未完成的 key 直接 POST 立即到账**真积分**
//! （`pick_skill` +1000 落进永久余额）；重复上报返回 `alreadyCompleted=true`，
//! 幂等不重复加分。所以「领取」就是对该账号全部未完成的 key 逐个上报。
//!
//! ── 结算记忆（插件形态下存台账）──────────────────────────────
//! 这 8 条是**一次性**福利：全部领完就永远是这个结论。结算后把快照写进账号
//! 台账（`ctx.ledger_put`），之后的查询与领取**零上游请求**；手动「查询任务」
//! 按钮带 `refresh = true` 强制实查（新事实会覆盖或清除记忆）。
//!
//! ── 为什么只挂手动入口，不进自动签到框架 ────────────────────
//! 新手任务是一次性福利，签到是每日动作；把一次性领取塞进每日调度，会让之后
//! 每一轮定时签到都白打一次查询接口。

use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::client;
use super::credentials;

const TASKS_PATH: &str = "/api/v1/onboarding/tasks";
const COMPLETE_PATH: &str = "/api/v1/onboarding/tasks/complete";

/// 台账的 kind（结算记忆）。
const LEDGER_KIND: &str = "onboarding";

/// 单个新手任务的静态信息（与 Loomy 客户端 task-registry 逐项一致，勿改 key）。
pub struct Task {
    /// 分组标题（对齐客户端任务面板的三段）
    pub group: &'static str,
    /// 上报 key（服务端契约，大小写敏感）
    pub key: &'static str,
    /// 展示标题
    pub title: &'static str,
    /// 完成奖励（积分）
    pub points: i64,
}

/// 8 个任务（合计 10000 积分）。key 与分值来自客户端 `TASK_POINTS`，一个都不能改。
pub const TASKS: [Task; 8] = [
    Task { group: "初识 Loomy", key: "first_message", title: "发送你的第一条消息", points: 500 },
    Task { group: "初识 Loomy", key: "pick_skill", title: "试试选择一个技能", points: 1000 },
    Task { group: "初识 Loomy", key: "generate_ppt", title: "生成第一份 PPT", points: 1500 },
    Task { group: "打造你的专属 Loomy", key: "set_schedule", title: "设置定时任务", points: 1000 },
    Task { group: "打造你的专属 Loomy", key: "install_skill", title: "在技能广场安装一个技能", points: 1500 },
    Task { group: "打造你的专属 Loomy", key: "configure_remote", title: "配置远程控制", points: 1000 },
    Task { group: "遇见你的 AI 搭子", key: "create_soul", title: "创建你的第一个搭子", points: 1500 },
    Task { group: "遇见你的 AI 搭子", key: "share_soul", title: "把搭子分享给朋友", points: 2000 },
];

/// 任务总分（界面「已获得 x / 共 y」的分母）
pub const TOTAL_POINTS: i64 = 10000;

/// 上游信封判定：`000000` 放行；登录失效 401；其余 502 带 `desc`。
fn ensure_success(payload: &Value, what: &str) -> Result<(), (u16, String)> {
    let code = client::business_code(payload);
    if client::is_auth_error_code(&code) {
        return Err((401, "Loomy 登录态已失效，请重新登录后再试".to_string()));
    }
    if code != client::SUCCESS_CODE {
        let message = client::upstream_message(payload);
        return Err((
            502,
            if message.is_empty() {
                format!("{what}失败（上游 code={code}）")
            } else {
                format!("{what}失败：{message}")
            },
        ));
    }
    Ok(())
}

/// 拉一次任务完成表：返回值按 [`TASKS`] 顺序对齐（未知 key 忽略，缺省未完成 ——
/// earned 不信任上游回传、自己算）。
async fn fetch_done(
    client: &reqwest::Client,
    credentials: &credentials::Credentials,
) -> Result<Vec<bool>, (u16, String)> {
    let payload = client::token_request(
        client,
        "GET",
        TASKS_PATH,
        &credentials.session,
        None,
        "新手任务查询",
    )
    .await
    .map_err(|message| (502, message))?;
    ensure_success(&payload, "新手任务查询")?;
    let tasks = payload.pointer("/data/tasks").cloned().unwrap_or(Value::Null);
    Ok(TASKS
        .iter()
        .map(|task| tasks.get(task.key).and_then(Value::as_bool).unwrap_or(false))
        .collect())
}

/// 按注册中心输出任务行（界面直接渲染的形状）。
fn task_rows(done: &[bool]) -> Vec<Value> {
    TASKS
        .iter()
        .zip(done.iter())
        .map(|(task, done)| {
            json!({
                "key": task.key,
                "title": task.title,
                "group": task.group,
                "points": task.points,
                "done": done,
            })
        })
        .collect()
}

fn earned_of(done: &[bool]) -> i64 {
    TASKS
        .iter()
        .zip(done.iter())
        .filter(|(_, done)| **done)
        .map(|(task, _)| task.points)
        .sum()
}

/// 读结算记忆（没有 / 读不懂 → None）。
async fn settled_snapshot(ctx: &PluginCtx, account: &str) -> Option<Value> {
    ctx.ledger_get(account, LEDGER_KIND)
        .await
        .ok()
        .filter(Value::is_object)
}

/// 读一条账号的凭证（`account` 空串 = 队首账号）。
async fn load_credentials(
    ctx: &PluginCtx,
    account: &str,
) -> Result<credentials::Credentials, (u16, String)> {
    if account.is_empty() {
        return Err((400, "缺少账号 id".to_string()));
    }
    let found = ctx
        .store_get(super::ID, account)
        .await
        .map_err(|error| (404, format!("未找到 Loomy 账号：{error}")))?;
    credentials::from_secret(&found).map_err(|message| (400, message))
}

/// 用记忆回答一次状态查询。
fn serve_memory(snapshot: &Value) -> Value {
    json!({
        "tasks": snapshot.get("tasks").cloned().unwrap_or(Value::Null),
        "earned": snapshot.get("earned").cloned().unwrap_or(Value::Null),
        "total": snapshot.get("total").cloned().unwrap_or(Value::Null),
        "unclaimed": 0,
        "settled": true,
        "settledAt": snapshot.get("at").cloned().unwrap_or(Value::Null),
    })
}

/// 按响应自己的结论落记忆，并把结果如实标回响应。
///
/// `settled == true` ⇒ 写入并返回快照本体，同时在响应里补 `settledAt`；否则
/// **清掉**旧快照（上游出了新事实）。结论与旧快照一致（任务行逐字相同）时
/// 沿用旧的 `at` —— 手动刷新不该把「结算于」改成「刚才查过」。
async fn remember(
    ctx: &PluginCtx,
    account: &str,
    payload: &mut Value,
    previous: Option<&Value>,
) {
    let settled = payload.get("settled").and_then(Value::as_bool) == Some(true);
    if !settled {
        if let Err(error) = ctx.ledger_put(account, LEDGER_KIND, &Value::Null).await {
            agent2api_provider_sdk::log_info!(
                "[Loomy] 账号 {account}：结算记忆清除失败（{error}，不影响本次结果）"
            );
        }
        return;
    }
    let tasks = payload.get("tasks").cloned().unwrap_or(Value::Null);
    let at = match previous {
        Some(old) if old.get("tasks") == Some(&tasks) => old
            .get("at")
            .and_then(Value::as_i64)
            .unwrap_or_else(crate::http::now_ms),
        _ => crate::http::now_ms(),
    };
    let snapshot = json!({
        "at": at,
        "tasks": tasks,
        "earned": payload.get("earned").cloned().unwrap_or(Value::Null),
        "total": payload.get("total").cloned().unwrap_or(Value::Null),
    });
    if let Err(error) = ctx.ledger_put(account, LEDGER_KIND, &snapshot).await {
        agent2api_provider_sdk::log_warn!(
            "[Loomy] ⚠️ 账号 {account}：新手任务结算记忆落库失败（本次结果不受影响，下次查询会重试）：{error}"
        );
    }
    if let Some(object) = payload.as_object_mut() {
        object.insert("settledAt".to_string(), json!(at));
    }
}

/// 查询任务状态。
///
/// `refresh = false` 且记忆里已结算 ⇒ 直接回答，**零上游请求**。
pub async fn get_tasks(ctx: &PluginCtx, account: &str, refresh: bool) -> Result<Value, (u16, String)> {    let previous = settled_snapshot(ctx, account).await;
    if !refresh {
        if let Some(snapshot) = previous.as_ref() {
            return Ok(serve_memory(snapshot));
        }
    }
    let credentials = load_credentials(ctx, account).await?;
    let client = crate::http::control_client(None, &ctx.env.proxy);
    let done = fetch_done(&client, &credentials).await?;
    let unclaimed = done.iter().filter(|done| !**done).count();
    let mut payload = json!({
        "tasks": task_rows(&done),
        "earned": earned_of(&done),
        "total": TOTAL_POINTS,
        "unclaimed": unclaimed,
        "settled": unclaimed == 0,
    });
    remember(ctx, account, &mut payload, previous.as_ref()).await;
    Ok(payload)
}

/// 上报一个 key。成功返回「是否此前已完成」（上游幂等标记）。
async fn claim_one(
    client: &reqwest::Client,
    credentials: &credentials::Credentials,
    key: &str,
) -> Result<bool, (u16, String)> {
    let body = json!({ "key": key });
    let payload = client::token_request(
        client,
        "POST",
        COMPLETE_PATH,
        &credentials.session,
        Some(&body),
        "新手任务领取",
    )
    .await
    .map_err(|message| (502, message))?;
    ensure_success(&payload, "新手任务领取")?;
    Ok(payload
        .pointer("/data/alreadyCompleted")
        .and_then(Value::as_bool)
        .unwrap_or(false))
}

/// 一键领取（或按 key 领单条）。
///
/// 先查完成表，再对**未完成**的 key 串行上报 —— 串行理由与签到一致（多请求
/// 并发打同一个上游没有收益，还容易撞风控）；某 key 失败不中断其余（逐行带
/// 原因返回），但**登录失效例外**：后面每个 key 都会以同样方式失败，立即整体 401。
/// 已完成的 key 不发请求，所以这条命令天然幂等，重复点不会重复加分。
///
/// `only` 非空时只领那一个 key（`Provider::claim` 的逐条入口）。
pub async fn claim(ctx: &PluginCtx, account: &str, only: &str) -> Result<Value, String> {
    if let Some(snapshot) = settled_snapshot(ctx, account).await {
        return Ok(settled_claim_response(&snapshot));
    }
    let credentials = load_credentials(ctx, account).await.map_err(|(_, message)| message)?;
    let client = crate::http::control_client(None, &ctx.env.proxy);
    let before = fetch_done(&client, &credentials).await.map_err(|(_, message)| message)?;

    let mut rows = Vec::new();
    let mut claimed = 0_i64;
    let mut failed = 0_i64;
    let mut claimed_points = 0_i64;
    let mut after = before.clone();
    for (index, task) in TASKS.iter().enumerate() {
        if before[index] {
            continue;
        }
        if !only.is_empty() && only != task.key {
            continue;
        }
        match claim_one(&client, &credentials, task.key).await {
            Ok(already) => {
                claimed += 1;
                claimed_points += task.points;
                after[index] = true;
                rows.push(json!({
                    "key": task.key, "ok": true, "already": already, "points": task.points,
                }));
                if already {
                    agent2api_provider_sdk::log_info!(
                        "[Loomy] 账号 {}: 「{}」此前已完成，跳过重复加分",
                        credentials.id,
                        task.title
                    );
                } else {
                    agent2api_provider_sdk::log_info!(
                        "[Loomy] 账号 {}: 领取「{}」+{} 积分",
                        credentials.id,
                        task.title,
                        task.points
                    );
                }
            }
            Err((401, message)) => return Err(message),
            Err((_, message)) => {
                failed += 1;
                agent2api_provider_sdk::log_info!(
                    "[Loomy] 账号 {}: 领取「{}」失败: {message}",
                    credentials.id,
                    task.title
                );
                rows.push(json!({ "key": task.key, "ok": false, "error": message }));
            }
        }
    }

    let unclaimed = after.iter().filter(|done| !**done).count();
    let mut payload = json!({
        "results": rows,
        "claimed": claimed,
        "failed": failed,
        "claimedPoints": claimed_points,
        "tasks": task_rows(&after),
        "earned": earned_of(&after),
        "total": TOTAL_POINTS,
        "unclaimed": unclaimed,
        "settled": unclaimed == 0,
    });
    remember(ctx, account, &mut payload, None).await;
    Ok(payload)
}

/// 记忆命中时领取接口的回答：这次**一次上游都没打**，因此没有一条 results。
fn settled_claim_response(snapshot: &Value) -> Value {
    let mut payload = serve_memory(snapshot);
    if let Some(object) = payload.as_object_mut() {
        object.insert("results".to_string(), json!([]));
        object.insert("claimed".to_string(), json!(0));
        object.insert("failed".to_string(), json!(0));
        object.insert("claimedPoints".to_string(), json!(0));
    }
    payload
}
