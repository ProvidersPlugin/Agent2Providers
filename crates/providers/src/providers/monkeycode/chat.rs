//! MonkeyCode 会话转发入口（装配层）：建任务 → 连任务流 → 翻译成 OpenAI 流。
//!
//! ── 这一层解决什么问题 ──────────────────────────────────────
//! 一次客户端请求在上游是**多步会话**（`adapter.rs` 的 `is_stateful` 为 true，
//! 编排层分流到这里）：
//! ```text
//!   ① credentials 快照（session + image_id；缺 image_id 直接报可读错误）
//!   ② 模型名 → 目录条目（upstreamId / cliName）        models::resolve_task_model
//!   ③ OpenAI messages → prompt（system 单列）          task::build_prompt
//!   ④ POST /api/v1/users/tasks 建任务                  task::create_task
//!   ⑤ 连 wss://…/tasks/stream?id=&mode=new             stream::connect
//!   ⑥ 发 auto-approve + user-input                     stream::start_task
//!   ⑦ 消费 ACP 事件 → 本网关的 OpenAI 兼容流            stream::drive_stream
//!   ⑧ PUT /api/v1/users/tasks/stop 收尾（**必须**）     client::stop_task
//! ```
//! 四 ~ 六步失败时**还没有给客户端任何字节**，因此按普通网关错误返回（带状态码，
//! 编排层照常换账号 / 透传）；七步之后头已发出，错误只能进流内（见 `stream.rs`）。
//!
//! ── 第八步为什么是必须的（10811 的根因）────────────────────────
//! 上游的任务状态只有 `pending` / `processing` / `finished` / `error`，而并发
//! 额度按「`pending` + `processing` 的任务数」算（上游 `entx.TaskConcurrencyHook`，
//! 业务码 **10811**）。关键语义：**`task-ended` 只结束本轮，不结束任务** ——
//! 任务会停在 `processing` 给多轮复用留窗口。本网关每个下游请求都新建一个
//! 任务，因此每轮收尾都必须显式停止；否则账号槽位会被占到 VM 空闲回收
//! （默认 3 天），第二次请求就稳定撞 10811。
//!
//! 停止动作覆盖的路径（漏一条就漏一个槽位）：
//!   - `stream.rs::close_and_stop`：四个终态出口 + 两处「客户端已断开」；
//!   - `stream.rs::StopOnDrop`：future 被丢弃（abort / 运行时关闭）的兜底；
//!   - 本文件的 ⑤ / ⑥ 失败：任务已建出来但流没起来；
//!   - `create_task_with_recovery`：撞 10811 时清理历史残留。
//!
//! ── 分工（哪些判断不属于这里）───────────────────────────────
//!   账号选路、限额冷却、provider 轮询、telemetry 记账都在编排层
//!   （`upstream::provider_loop`）；单请求内每一步的协议细节在 `task.rs` /
//!   `stream.rs`。本文件只做装配与三个小决策：**流式还是聚合**、**哪些失败
//!   在返回前发生**、**撞 10811 时怎么自愈**。
//!
//! ── 非流式怎么处理（与 catpaw / kuku 同一取舍）────────────────
//! `stream=false` 时仍走同一条任务流（事件流是上游唯一产出通道），由
//! `stream::drive_aggregate` 把同一份翻译状态聚合成完整 `chat.completion`。
//! 与流式路径唯一的差别是错误可以带状态码返回（没下发过头），因此额度 / 鉴权
//! 这类错误在非流式下仍能走到编排层的账号轮换；流式下只能在流内收尾。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic；持锁不跨 await
//! （本文件不持任何锁，账号读取是一次同步快照）。

use agent2api_provider_protocol as proto;
use agent2api_provider_protocol::WireProxy as ResolvedProxy;
use agent2api_provider_sdk::{ConversationSink, PluginCtx};
use serde_json::Value;

use crate::errors::GatewayError;
use crate::logging;

use super::credentials::MonkeyCodeCredentials;
use super::region::Region;
use super::{client, credentials, inflight, models, stream, task};

/// 转发入口（`mod.rs` 的 `Provider::conversation` 转调）。
pub async fn run_chat(
    ctx: &PluginCtx,
    region: Region,
    req: proto::ConversationRequest,
    sink: ConversationSink<'_>,
) -> Result<(), String> {
    run_chat_inner(ctx, region, req, sink)
        .await
        .map_err(|error| error.message)
}

async fn run_chat_inner(
    ctx: &PluginCtx,
    region: Region,
    req: proto::ConversationRequest,
    mut sink: ConversationSink<'_>,
) -> Result<(), GatewayError> {
    let account_id = req.account.as_str();
    let body = &req.body;
    let proxy = req.proxy.clone();
    let stream_requested = req.stream;
    let record = if account_id.is_empty() {
        None
    } else {
        let found = crate::store::record(ctx, region.provider_id(), account_id).await?;
        if found.is_null() {
            return Err(GatewayError::with_status(
                401,
                format!(
                    "MonkeyCode {} 账号 {account_id} 不存在（请重新添加）",
                    region.label()
                ),
            ));
        }
        Some(found)
    };
    let credentials = credentials::from_record(record.as_ref())?;
    if !credentials.ready_for_task() {
        return Err(GatewayError::with_status(
            400,
            format!(
                "MonkeyCode {} 账号缺少创建任务所需的 image_id（VM 镜像 UUID）：\
                 请在账号页编辑该账号填入（浏览器登录后，DevTools → Network → \
                 POST /api/v1/users/tasks 请求里的 image_id），或先在官网跑一个任务让网关自动发现",
                region.label()
            ),
        ));
    }
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if requested.is_empty() {
        return Err(GatewayError::with_status(400, "请求缺少 model"));
    }
    let target = models::resolve_task_model(region, &requested).ok_or_else(|| {
        GatewayError::with_status(
            503,
            format!(
                "MonkeyCode 模型目录里找不到模型「{requested}」：请在模型管理页刷新清单后重试"
            ),
        )
    })?;
    let prompt = task::build_prompt(body.get("messages"))?;
    logging::verbose(
        "[MonkeyCode]",
        &format!(
            "会话转发 model={} → upstream={} cli={} stream={} account={} region={}",
            requested,
            target.model,
            target.cli_name,
            stream_requested,
            if account_id.is_empty() { "(队首账号)" } else { account_id },
            region.id(),
        ),
    );
    // ① 建任务（失败在返回前：错误带状态码，编排层照常换账号 / 透传）
    let task_id =
        create_task_with_recovery(region, &credentials, &target, &prompt, proxy.as_ref()).await?;
    // 登记为「本进程正在驱动」：10811 自愈时据此跳过，避免停掉并发请求
    // 正在用的任务（见 `inflight.rs` 的说明）。
    inflight::register(&task_id);
    // ② 连任务流并发出起始交互（同上，失败在返回前）
    //
    // ── 这两步失败必须回头停止任务 ──────────────────────────────
    // 任务在上一步已经建出来了（上游已分配并发槽位）。连不上 WS / 发不出起始
    // 消息时，若不停止它，这个账号的槽位会被占到 VM 空闲回收（默认 3 天），
    // 之后每次建任务都撞 10811。停止失败不改变本次结论（错误照常返回）。
    let mut connection = match stream::connect(region, &credentials, &task_id, proxy.as_ref()).await {
        Ok(connection) => connection,
        Err(error) => {
            client::stop_task(region, &credentials.session, &task_id, proxy.as_ref()).await;
            inflight::unregister(&task_id);
            return Err(error);
        }
    };
    if let Err(error) = stream::start_task(&mut connection, &prompt.content).await {
        connection.close().await;
        client::stop_task(region, &credentials.session, &task_id, proxy.as_ref()).await;
        inflight::unregister(&task_id);
        return Err(error);
    }
    let translator = stream::Translator::new(
        format!("chatcmpl-{task_id}"),
        requested,
        include_usage(body),
    );
    if stream_requested {
        // ── 流式：先报头，再推帧（协议里没有「返回一个流」的槽位）──────
        // 网关靠 head 决定状态码与翻译机；头一到，后面就只剩 chunk。
        sink.head(proto::StreamHead {
            status: 200,
            headers: [(
                "content-type".to_string(),
                "text/event-stream".to_string(),
            )]
            .into_iter()
            .collect(),
            // 上游是 OpenAI 兼容帧（`translate.rs` 翻译出来的），不是原生 SSE
            upstream: proto::UpstreamKind::Chat,
        })
        .await
        .map_err(|error| GatewayError::with_status(499, error))?;
        stream::drive_stream(connection, translator, &mut sink).await?;
        Ok(())
    } else {
        // ── 非流式：聚合完再一次性推（此时还没报头，失败可以带状态码）──
        let completion = stream::drive_aggregate(connection, translator).await?;
        let payload = serde_json::to_string(&completion).map_err(|error| {
            GatewayError::with_status(500, format!("MonkeyCode 聚合结果序列化失败：{error}"))
        })?;
        sink.head(proto::StreamHead {
            status: 200,
            headers: [(
                "content-type".to_string(),
                "application/json".to_string(),
            )]
            .into_iter()
            .collect(),
            upstream: proto::UpstreamKind::Chat,
        })
        .await
        .map_err(|error| GatewayError::with_status(499, error))?;
        sink.send(payload.as_bytes())
            .await
            .map_err(|error| GatewayError::with_status(499, error))?;
        Ok(())
    }
}

/// 建任务；撞上「已有运行中的任务」（code 10811）时**自愈一次**。
///
/// ── 为什么需要这一步（而不是直接报错给用户）────────────────────
/// 10811 的判定是「该账号 `pending` + `processing` 的任务数已达上限」。本网关
/// 自己每轮都会停止任务（见 `stream.rs::close_and_stop`），所以 10811 的常见
/// 来源是**网关没能停掉的历史残留**：
///   - 修复前版本的请求（那时根本不停止）；
///   - 网关进程被强杀 / 崩溃，收尾代码没跑完；
///   - 建任务成功但连 WS 失败这类路径（同样会留下任务）。
/// 这些都是**本账号自己的僵尸任务**，用户在官网手动点「停止」也能解决 ——
/// 与其让用户去点，不如在这里自动清理：列出运行中的任务 → 逐个停止 → 重试
/// 一次建任务。
///
/// ── 边界（什么时候不自愈）────────────────────────────────────
///   - 只在**业务码恰好是 10811** 时触发（别的错误各有各的处置）；
///   - 只清理**列表里查到的**运行中任务；查不到（网络 / 结构变化）就原样报错，
///     不自作主张；
///   - 只重试**一次**。第二次仍 10811 说明不是残留问题（比如账号真在上限），
///     把上游原文透传出去，让用户自己判断。
async fn create_task_with_recovery(
    region: Region,
    credentials: &MonkeyCodeCredentials,
    target: &models::TaskTarget,
    prompt: &task::TaskPrompt,
    proxy: Option<&ResolvedProxy>,
) -> Result<String, GatewayError> {
    let first = task::create_task(region, credentials, target, prompt, proxy).await;
    let Err(error) = first else {
        return first;
    };
    // 只认 10811（见模块头的码表；`upstream_code` 由 `task.rs` 挂上）
    if error.upstream_code != Some(10811) {
        return Err(error);
    }
    let stale = client::list_running_tasks(region, &credentials.session).await;
    // 排除本进程正在驱动的任务：同一个账号上可能有别的请求在跑（并发转发），
    // 停掉它们等于把别人的流掐断（见 `inflight.rs` 的说明）。
    let stale: Vec<String> = stale
        .into_iter()
        .filter(|id| !inflight::contains(id))
        .collect();
    if stale.is_empty() {
        return Err(error);
    }
    logging::log(
        "[MonkeyCode]",
        &format!(
            "账号命中 10811（已有运行中的任务），自动清理 {} 个残留任务后重试：{}",
            stale.len(),
            stale.join(", ")
        ),
    );
    let mut stopped = 0usize;
    for id in &stale {
        if client::stop_task(region, &credentials.session, id, proxy).await {
            stopped += 1;
        }
    }
    if stopped == 0 {
        // 一个都没停成：残留清理失败，把原始 10811 报回去（别把「清理失败」
        // 表现为「重试失败」，用户要看到的是真正卡住的原因）
        return Err(error);
    }
    logging::verbose(
        "[MonkeyCode]",
        &format!("已停止 {stopped}/{} 个残留任务，重试建任务", stale.len()),
    );
    task::create_task(region, credentials, target, prompt, proxy).await
}

/// `stream_options.include_usage === true`（只认布尔真值，与 catpaw 同口径：
/// 字符串 / 数字不算，少一帧比多一帧更容易被客户端发现）。
fn include_usage(body: &Value) -> bool {
    body.pointer("/stream_options/include_usage")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}
