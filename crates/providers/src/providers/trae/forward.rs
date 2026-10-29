//! Trae 的转发（**有状态路径**：`is_stateful() = true`）。
//!
//! ── 为什么本家不能走通用无状态路径 ─────────────────────────
//! 无状态路径的形状是「通用层序列化 body 原样发出、响应按 OpenAI 的 SSE 逐行
//! **透传」**（`upstream::ForwardStream` → `sse::ReasoningCoalescer`，中间没有
//! 任何"按家翻译帧"的钩子 —— 全仓只有 `custom/forward.rs` 那一个协议翻译器，
//! 而它要的两把凭证（`InFlightGuard` / `ConnectionGuard`）不会交给适配器）。
//! Trae 的出站方向两头都不合这个假设：
//!
//!   * 请求体是自定义白名单信封（`payload::prepare_body`，多带字段是被拒而不是
//!     被忽略）；
//!   * 响应是另一套 SSE 方言：**没有 `[DONE]`**（结束帧是 `event: done`）、
//!     `token_usage` 是欠到下一帧的、`event: error` 之后上游**还会继续发帧**。
//!
//! 所以本家与 Accio / Qoder 同一处置：实现 `forward_conversation`，自己发请求、
//! 自己翻帧、自己产出对客户端可见的字节。
//!
//! ── 首包门（本家最要紧的一段）──────────────────────────────
//! 上游的额度/凭证类失败**不带 HTTP 状态**：HTTP 200 + SSE 里一条
//! `event: error`（1005 / 4008 / 4001…）。不拦的话客户端拿到的是"成功但没话",
//! 而网关这边一次失败连账号都不换。做法是先把流头预读出来（`prefetch_head`）：
//! 第一帧是错误就**在这里**判成带状态的 `GatewayError` 返回，一个字节都不下发；
//! 第一帧是内容才交回 `ForwardOutcome::Stream`。Accio / Qoder 同一手法，
//! 而 CPA 那边是插件自己做过一遍的同类修正。
//!
//! 状态码映射表（本家特有，别家照抄会错）：
//!   `SessionDead` → 401、`SoftRate` / `PlanLimit` → 429、`InputTooLarge` → 413、
//!   `ModelUnavailable` → 404、其余 → 502。
//! 为什么 1005/4008 在 agent2api 这边记 429 而不是参考实现内部那 12 小时冷却：
//! 429 是宿主唯一认识的「换账号」信号（有状态路径不自动换号，见下），把冷却
//! 时长留在自己手里更可控 —— 落盘标记走 `mark_rate_limited`，与另外几家同源。
//!
//! ── 有状态路径**不会**换账号重试（必须自己续期）──────────────
//! `provider_loop::attempt_stateful` 对错误是「一律透传」：不换号、不冷却、
//! 不重试。所以两件别家由编排层代劳的事在这里自己做：
//!   ① 临期主动续期（发送前）；② 真被拒 401 时**强制续一次再发一次**。
//! 第二条不能省：Trae 的 refreshToken 一次一换，被拒时手里那份 accessToken
//! 多半是"服务端已吊销"而不是"过期"，不重发就直接把一次可救的调用报成失败。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic；网络不在锁内。

use std::io;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::{Value, json};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ConversationSink, PluginCtx};
use crate::http as egress;
use agent2api_provider_protocol::WireProxy as ResolvedProxy;
use crate::telemetry::RequestTelemetry;
use crate::stall;
use crate::errors::GatewayError;
use crate::logging;

use super::credentials::Credential;
use super::errors::ErrorKind;
use super::headers::{IDE_VERSION, solo_headers};
use super::payload;
use super::stream::{Frame, SoloStream};
use super::{AGENT_BASE_URL, CHAT_PATH, PROVIDER_ID};

/// 流式的读空闲上限（与其它几家同一个配置源，不是本家自己拍的数）。
fn stream_idle() -> Duration {
    Duration::from_millis(crate::timeouts::current().stream_idle_ms())
}

/// 一次出站请求（URL + 头 + 已改写的 body）。
pub struct Plan {
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// 客户端要的那个模型名（回显给客户端用；上游收的裸名在 `body` 里）。
    pub requested_model: String,
    pub body: Value,
}

/// 构造出站请求。
///
/// `resolved` 传空：**广告名带 `-solo` 后缀，出站名要剥回来**，这件事
/// `prepare_body` 内部就做（它只剥一层，所以真以 `-solo` 结尾的 config 仍往返）。
pub fn build_plan(credential: &Credential, body: &Value, base: &str) -> Result<Plan, GatewayError> {
    if credential.access_token.trim().is_empty() {
        return Err(GatewayError::with_status(401, "Trae 账号缺少 accessToken，无法转发（请重新登录）"));
    }
    // 谱系闸门（**转发侧也放一份**，不只是落账号时）：账号可能是从迁移导入或
    // 手工写库进来的，那条路不经过 `add_trae_account`。Intl 凭据打到 CN host
    // 只会收到一句没头没尾的 401，与其让人猜，不如在这里说清楚。
    if super::credentials::is_intl_variant(credential.variant()) {
        return Err(GatewayError::with_status(400, super::credentials::INTL_UNSUPPORTED));
    }
    let requested = body.get("model").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let identity = super::headers::HeaderIdentity {
        access_token: credential.access_token.trim(),
        uid: credential.uid.trim(),
        machine_id: credential.machine_id.trim(),
        device_id: credential.device_id.trim(),
    };
    // 上游恒 `stream:true`（`prepare_body` 写死），Accept 因此恒 event-stream。
    let headers = solo_headers(&identity, true).into_iter().collect();
    Ok(Plan {
        url: format!("{}{CHAT_PATH}", base.trim_end_matches('/')),
        headers,
        requested_model: requested,
        body: payload::prepare_body(body, credential.variant(), ""),
    })
}

/// 发一次请求（**不设总超时**：长推理会被总超时截断，读空闲由 `idle_guard` 兜）。
async fn send(plan: &Plan, proxy: Option<&ResolvedProxy>) -> Result<reqwest::Response, GatewayError> {
    // **不设 reqwest 总超时**：`timeout(0)` 不是"不限时"而是"立刻超时"
    // （实测报 operation timed out，症状是随机 502），而不带总超时的长流
    // 才符合本家的形状 —— 读空闲由下面的 `idle_guard` 管，整流时长不该被截。
    let mut request = egress::stream_client(proxy, "direct")
        .post(&plan.url)
        .header("User-Agent", format!("Trae/{IDE_VERSION} antigravity-cockpit-tools"));
    for (name, value) in &plan.headers {
        request = request.header(name.as_str(), value.clone());
    }
    let response = request
        .json(&plan.body)
        .send()
        .await
        .map_err(|error| GatewayError::with_status(502, format!("Trae 请求发不出去：{}", egress::describe_chain(&error))))?;
    Ok(response)
}

/// 限额标记的上下文（有状态路径不换号，所以冷却要自己记）。
///
/// ── 迁移时改了什么 ────────────────────────────────────────
/// 网关那份持 `Arc<AccountStore>`（同步写账号库）。插件里对应的是
/// `ctx.store_mark_limited(...)` —— 一次 RPC，异步。因此 `punish` 变成 async，
/// 调用点跟着 await。`ctx` 是借用（`conversation` 是推模型，不需要 'static）。
pub struct Limit<'a> {
    pub ctx: &'a PluginCtx,
    pub account_id: String,
    pub model: String,
}

impl Limit<'_> {
    /// 按错误类别落冷却/禁用标记。返回的是**给客户端的状态码**。
    async fn punish(&self, kind: ErrorKind, status: u16, message: &str) -> u16 {
        if self.account_id.is_empty() {
            return status;
        }
        match kind {
            // 软限流与计划限额：冷却这个账号（上游不给恢复时刻，网关落兜底时长）
            ErrorKind::SoftRate | ErrorKind::PlanLimit => {
                if let Err(error) = self
                    .ctx
                    .store_mark_limited(&self.account_id, &self.model, None)
                    .await
                {
                    logging::verbose(
                        "[Trae]",
                        &format!("标记账号限额失败（不影响本次转发）：{error}"),
                    );
                    return status;
                }
                logging::log("[Trae]", &format!("⚠️ 账号 {} 对模型 {} 已限额（{status}），进入冷却：{message}", self.account_id, self.model));
            }
            // 会话失效：上游明说这串凭据不行了，落同样的标记让它下一轮被跳过
            ErrorKind::SessionDead => {
                if let Err(error) = self
                    .ctx
                    .store_mark_limited(&self.account_id, &self.model, None)
                    .await
                {
                    logging::verbose(
                        "[Trae]",
                        &format!("标记账号失效失败（不影响本次转发）：{error}"),
                    );
                    return status;
                }
                logging::log("[Trae]", &format!("⚠️ 账号 {} 会话失效（{status}），请重新登录", self.account_id));
            }
            // 过大 / 通道不可用 / 其它：**请求级**，一个字都不写账号库
            _ => {}
        }
        status
    }
}

/// 错误类别 → 客户端可见状态码（映射表见模块头）。
///
/// `pub(crate)` 给 `usage.rs` 复用：积分/额度那条链的 401/429 判档与转发用的是
/// **同一张表**（`errors::classify` 的产出），两处各写一份的话，"余额查询报 401
/// 时上层该不该刷新重试"就会和转发侧悄悄不一致。
pub(crate) fn status_for(kind: ErrorKind) -> u16 {
    match kind {
        ErrorKind::SessionDead => 401,
        ErrorKind::SoftRate | ErrorKind::PlanLimit => 429,
        ErrorKind::InputTooLarge => 413,
        ErrorKind::ModelUnavailable => 404,
        ErrorKind::NotFound => 404,
        _ => 502,
    }
}

/// 非 2xx 响应 → 网关错误（错误体只读一次，读完就丢）。
async fn http_error(status: u16, response: reqwest::Response, limit: &Limit<'_>) -> GatewayError {
    let text = response.text().await.unwrap_or_default();
    let head: String = text.chars().take(240).collect();
    let kind = super::errors::classify(status, &head);
    let message = format!("上游返回 {status}: {head}");
    let status = limit.punish(kind, status_for(kind), &message).await;
    GatewayError::with_status(i32::from(status), if head.is_empty() { format!("Trae 上游返回 HTTP {status}") } else { message })
}

/// 流内错误帧 → 网关错误（首包门专用：此时一个字节都还没下发）。
async fn frame_error(code: i64, message: &str, limit: &Limit<'_>) -> GatewayError {
    let kind = super::errors::stream_error_kind(code, message);
    let text = format!("solo error code={code} msg={message}");
    let status = limit.punish(kind, status_for(kind), &text).await;
    GatewayError::with_status(i32::from(status), format!("Trae 上游错误：{text}")).with_code(PROVIDER_ID)
}

/// 预读流头：返回「已读出的内容帧」+「剩下的流」。第一帧是错误就直接 Err。
///
/// 半行换手与 accio 同一处置：缓冲区里没成行的尾巴必须拼回流头，否则被切在
/// chunk 边界的那一帧会静默丢内容（这是本文件最难查的一类 bug，症状是"偶尔
/// 少开头几个字"）。
pub(crate) async fn prefetch_head(
    response: reqwest::Response,
    limit: &Limit<'_>,
) -> Result<(Vec<Frame>, futures::stream::BoxStream<'static, Result<bytes::Bytes, io::Error>>, SoloStream, ByteLines), GatewayError> {
    let source = response.bytes_stream().map(|item| item.map_err(|error| io::Error::other(egress::describe_chain(&error))));
    let mut source = stall::idle_guard(Box::pin(source), stream_idle());
    let mut scanner = ByteLines::default();
    // `id` / `created` 从这里开始就得有值：预读到的帧要原样补发给客户端。
    let mut stream = SoloStream::new(request_id(), logging::now_ms() / 1000);
    let mut frames: Vec<Frame> = Vec::new();
    while let Some(item) = source.next().await {
        let chunk = item.map_err(|error| GatewayError::with_status(502, format!("Trae 上游流式传输中断: {error}")))?;
        // ★ 一整段 chunk 处理完才返回 —— 中途 return 会把**同一段里后面的行**
        //   连同字节一起丢掉（那些行已经从 socket 读进来了，换手也换不回来）。
        //   上游经常把 `output`+`token_usage`+`done` 挤在同一段里，所以这不是
        //   理论问题：症状是"用量恒为 0"和"error 帧凭空消失"。
        for line in scanner.push(&chunk) {
            for frame in stream.feed_line(&line) {
                if let Frame::Error { code, message, .. } = &frame {
                    // 门只拦"第一帧就是错误"：内容已经出去过之后再报错，
                    // HTTP 200 已经发出去了，只能留在流里如实告诉客户端。
                    if frames.is_empty() {
                        return Err(frame_error(*code, message, limit).await);
                    }
                }
                frames.push(frame);
            }
            if stream.completed() { break; }
        }
        if stream.completed() {
            return Ok((frames, Box::pin(futures::stream::empty()), stream, scanner));
        }
        if !frames.is_empty() {
            return Ok((frames, source, stream, scanner));
        }
    }
    // 流在没有任何内容帧之前就结束了：交给下游按「空回答」收尾（不假装成功
    // 也不报错 —— 真异常在 HTTP 状态那一层已经判过了）。
    Ok((frames, source, stream, scanner))
}

use crate::sse_events::ByteLines;

/// 把一帧写成对客户端可见的 SSE 字节。
///
/// `model` 在这里改写成**客户端请求的那个名字**：上游帧里 `model` 恒为空串
/// （参考实现如此，本家的 `SoloStream` 逐字节对齐它也如此），而 OpenAI 客户端
/// 拿空 model 会显示成"未知模型"。这与 `sse_model_rewrite()` 无关 —— 那条是
/// 通用透传路径的钩子，本家自己发字节，所以自己改（与 Qoder / Accio 同一处置）。
fn render(frame: &Frame, requested: &str) -> Option<String> {
    match frame {
        Frame::Chunk(chunk) => {
            let mut chunk = chunk.clone();
            if !requested.is_empty() {
                if let Some(object) = chunk.as_object_mut() {
                    object.insert("model".to_string(), Value::String(requested.to_string()));
                }
            }
            Some(format!("data: {chunk}\n\n"))
        }
        Frame::Error { code, message, .. } => Some(format!("data: {}\n\n", json!({"error": {"message": format!("solo error code={code} msg={message}"), "type": "upstream_error"}}))),
        Frame::Done => Some("data: [DONE]\n\n".to_string()),
    }
}

fn request_id() -> String {
    format!("chatcmpl-{}", logging::now_ms() * 1000 % 1_000_000_000_000_000)
}

/// 用一条凭据打一次上游并产出对客户端可见的 outcome（有状态路径的入口）。
/// 生产入口：打默认的上游主机。
pub(crate) async fn forward(
    ctx: &PluginCtx,
    req: proto::ConversationRequest,
    sink: &mut ConversationSink<'_>,
) -> Result<(), GatewayError> {
    forward_at(ctx, req, sink, AGENT_BASE_URL).await
}

/// 同上，但上游基址由调用方给。
///
/// 这一层参数**不是**为了好看：状态机（首包门、半行换手、usage 欠一帧、
/// error 之后继续读）只有在真 HTTP 上才验得到，而测试不能拿真上游去试 ——
/// Trae 每一次聊天都烧账号额度。基址做成入参（而不是读环境变量）是为了
/// 并发测试之间不互相踩：`std::env::set_var` 在同一进程里是全局可见的。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn forward_at(
    ctx: &PluginCtx,
    req: proto::ConversationRequest,
    sink: &mut ConversationSink<'_>,
    base: &str,
) -> Result<(), GatewayError> {
    let account_id = req.account.as_str();
    let body = &req.body;
    let stream = req.stream;
    let proxy = req.proxy.clone();
    // 插件里的遥测是空实现（见 `telemetry` 模块头）：调用点逐字保留。
    let telemetry = Arc::new(RequestTelemetry::new());
    if account_id.is_empty() {
        return Err(GatewayError::with_status(
            503,
            "没有可用的 Trae 账号：请在账号页添加并启用账号",
        ));
    }
    let record = crate::store::record(ctx, PROVIDER_ID, account_id).await?;
    if record.is_null() {
        return Err(GatewayError::with_status(401, "找不到该 Trae 账号"));
    }
    let mut credential = Credential::from_payload(&record).map_err(GatewayError::new)?;
    credential = super::adapter::renew_if_due(ctx, &record, &credential, proxy.as_ref()).await?;
    let limit = Limit {
        ctx,
        account_id: account_id.to_string(),
        model: body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    };
    let account_proxy = super::adapter::account_proxy(&record)?;
    let effective = proxy.or(account_proxy);

    let mut plan = build_plan(&credential, body, base)?;
    let capture = telemetry.capture();
    if let Some(capture) = capture.as_deref() {
        capture.reset_request(&plan.url, PROVIDER_ID, &plan.headers, body);
    }
    let mut response = send(&plan, effective.as_ref()).await?;
    // 401 只救一次：强制换发后重发（这条路径不换号，见模块头）。
    if response.status().as_u16() == 401 {
        let _ = response.text().await;
        credential = super::adapter::renew_forced(ctx, &record, &credential, effective.as_ref()).await?;
        plan = build_plan(&credential, body, base)?;
        response = send(&plan, effective.as_ref()).await?;
    }
    let status = response.status().as_u16();
    if status >= 400 {
        return Err(http_error(status, response, &limit).await);
    }
    if let Some(capture) = capture.as_deref() {
        capture.attach_response(status, response.headers());
    }

    if !stream {
        // 上游恒流式，因此非流式是"我们这边聚合"（与 Accio 同结构）。
        let completion =
            drive_aggregate(response, plan.requested_model.clone(), &telemetry, &limit).await?;
        let payload = serde_json::to_string(&completion).map_err(|error| {
            GatewayError::with_status(500, format!("Trae 聚合结果序列化失败：{error}"))
        })?;
        sink.head(proto::StreamHead {
            status: 200,
            headers: [("content-type".to_string(), "application/json".to_string())]
                .into_iter()
                .collect(),
            upstream: proto::UpstreamKind::Chat,
        })
        .await
        .map_err(|error| GatewayError::with_status(499, error))?;
        sink.send(payload.as_bytes())
            .await
            .map_err(|error| GatewayError::with_status(499, error))?;
        return Ok(());
    }

    let (prefetched, source, mut solo, scanner) = prefetch_head(response, &limit).await?;
    // 先报头（协议里没有「返回一个流」的槽位）
    sink.head(proto::StreamHead {
        status: 200,
        headers: [(
            "content-type".to_string(),
            "text/event-stream".to_string(),
        )]
        .into_iter()
        .collect(),
        upstream: proto::UpstreamKind::Chat,
    })
    .await
    .map_err(|error| GatewayError::with_status(499, error))?;
    let requested = plan.requested_model.clone();
    drive_stream(source, &mut solo, scanner, prefetched, &requested, &telemetry, sink).await;
    Ok(())
}

/// 流式下发（在 spawn 的后台任务里跑）。
async fn drive_stream(
    mut source: futures::stream::BoxStream<'static, Result<bytes::Bytes, io::Error>>,
    solo: &mut SoloStream,
    mut scanner: ByteLines,
    prefetched: Vec<Frame>,
    requested: &str,
    telemetry: &RequestTelemetry,
    sink: &mut ConversationSink<'_>,
) {
    let mut usage_seen: Option<Value> = None;
    for frame in prefetched {
        note(&frame, telemetry, &mut usage_seen);
        if let Some(text) = render(&frame, requested) {
            if sink.send(text.as_bytes()).await.is_err() {
                return;
            }
        }
    }
    if solo.completed() { return; }
    while let Some(item) = source.next().await {
        match item {
            Err(error) => {
                if let Some(line) = scanner.finish() {
                    for frame in solo.feed_line(&line) {
                        note(&frame, telemetry, &mut usage_seen);
                        if let Some(text) = render(&frame, requested) {
                            if sink.send(text.as_bytes()).await.is_err() { return; }
                        }
                    }
                }
                for frame in solo.flush_pending() {
                    note(&frame, telemetry, &mut usage_seen);
                    if let Some(text) = render(&frame, requested) {
                        if sink.send(text.as_bytes()).await.is_err() { return; }
                    }
                }
                let message = format!("Trae 上游流中断: {error}");
                telemetry.note_error(&message);
                let failure = crate::sse::error_bytes(&message);
                let _ = sink.send(&failure).await;
                let _ = sink.send(&crate::sse::done_bytes()).await;
                return;
            }
            Ok(chunk) => {
                for line in scanner.push(&chunk) {
                    for frame in solo.feed_line(&line) {
                        note(&frame, telemetry, &mut usage_seen);
                        if let Some(text) = render(&frame, requested) {
                            if sink.send(text.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    }
                    if solo.completed() { return; }
                }
            }
        }
    }
    if let Some(line) = scanner.finish() {
        for frame in solo.feed_line(&line) {
            note(&frame, telemetry, &mut usage_seen);
            if let Some(text) = render(&frame, requested) {
                if sink.send(text.as_bytes()).await.is_err() { return; }
            }
        }
    }
    // EOF 收尾：上游没给过 `done` 时由 `finish()` 补那一个 `[DONE]`。
    // 这里**不再自己添一个** —— `[DONE]` 必须恰好一个，而 `SoloStream` 已经
    // 保证「done / error / EOF 三条路各发一次且只一次」。
    for frame in solo.finish() {
        note(&frame, telemetry, &mut usage_seen);
        if let Some(text) = render(&frame, requested) {
            if sink.send(text.as_bytes()).await.is_err() {
                return;
            }
        }
    }
    let _ = &usage_seen;
}

/// 一帧的旁读（首帧时刻与用量上报），不改字节。
fn note(frame: &Frame, telemetry: &RequestTelemetry, usage_seen: &mut Option<Value>) {
    match frame {
        Frame::Chunk(chunk) => {
            telemetry.note_first_frame();
            // 上游的 `token_usage` 事件载荷本身是 `{"usage": {…}}`，而 `SoloStream`
            // 把它整包挂到下一帧的 `usage` 键上（与参考实现逐字节一致），所以这里
            // 取**内层**才对得上 `extract_usage` 的那三把键 —— 报外层的后果是
            // 账目静默为 0（`extract_usage` 找不到 `prompt_tokens` 就整个返回 None）。
            let inner = chunk.get("usage").and_then(|value| value.get("usage"));
            let usage = inner.or_else(|| chunk.get("usage")).or_else(|| chunk.get("token_usage"));
            if let Some(usage) = usage {
                *usage_seen = Some(usage.clone());
                telemetry.report_usage(usage);
            }
        }
        Frame::Error { code, message, .. } => {
            telemetry.note_error(&format!("solo error code={code} msg={message}"));
        }
        Frame::Done => {}
    }
}

/// 非流式：内部仍按流式拉全，再折成一个 `chat.completion`。
async fn drive_aggregate(
    response: reqwest::Response,
    requested: String,
    telemetry: &RequestTelemetry,
    limit: &Limit<'_>,
) -> Result<Value, GatewayError> {
    let source = response.bytes_stream().map(|item| item.map_err(|error| io::Error::other(egress::describe_chain(&error))));
    let mut source = stall::idle_guard(Box::pin(source), stream_idle());
    let mut scanner = ByteLines::default();
    let mut text = String::new();
    let mut observer = SoloStream::new(request_id(), logging::now_ms() / 1000);
    let mut failure = None;
    'read: while let Some(item) = source.next().await {
        let chunk = match item {
            Ok(chunk) => chunk,
            Err(error) => { failure = Some(error); break; }
        };
        for line in scanner.push(&chunk) {
            text.push_str(&line);
            text.push('\n');
            observer.feed_line(&line);
            if observer.completed() { break 'read; }
        }
    }
    if !observer.completed() {
        if let Some(line) = scanner.finish() {
            text.push_str(&line);
            text.push('\n');
        }
    }
    if let Some(error) = failure {
        return Err(GatewayError::with_status(502, format!("Trae 上游流式传输中断: {error}")));
    }
    match super::stream::aggregate(&text, &request_id(), logging::now_ms() / 1000) {
        Ok(mut completion) => {
            if !requested.is_empty() {
                if let Some(object) = completion.as_object_mut() {
                    object.insert("model".to_string(), Value::String(requested));
                }
            }
            if let Some(usage) = completion.get("usage") {
                telemetry.report_usage(usage);
            }
            Ok(completion)
        }
        // 流内错误：与流式同一张状态码表（这里换号是没用的，但**必须**带状态，
        // 否则客户端拿到的是 200 + 空内容 —— 参考实现摔过的正是这一种）。
        Err(error) => Err(frame_error(error.code, &error.message, limit).await),
    }
}

