//! KukuAI 对话转发：建会话 → 分配算力 → SSE 流式 → OpenAI 形状。
//!
//! ── 上游是**会话式三步走**（kuku2api 2026-09-13 实测，本模块照抄）────
//!   1. `POST /wenchain/genflowpro/sendmsg`（带 `bdstoken/uinfo/uk` 会话三件套）
//!      建会话，返回 `{session_id, reply_id}`；
//!   2. `POST /wenchain/genflow/idallochstr` 分配算力（+ `sessionswitch`，
//!      浏览器会调，容错 —— 失败不影响对话）；
//!   3. `POST /wenchain/genflowpro/sse/getchatcontent`（SSE）→ 流式文本。
//! SSE 帧是 `data: {type, data}` JSON，事件只有四类：
//!   `TEXT_BLOCK_DELTA`（`data.delta` 增量）/ `REPLY_END` / `DIALOGUE_END`
//!   （结束）/ `ERROR`（终止流，已发内容保留 —— 与参考实现一致）。
//!
//! ── 多轮与模型 ──────────────────────────────────────────────
//! 上游**没有多轮接续接口**：每次请求新建会话，历史拍平成单条 prompt
//! （`build_prompt`：system → `[系统指令]`、assistant → `[助手之前的回复]`，
//! 与参考实现同一口径）。模型参数 `model_name` 直接给上游（`models::resolve_model`
//! 归一，未知显式回退 `auto`）。工具调用第一版**不做**注入（上游有产品级人格
//! 保护，注入式协议不可靠，见 `mod.rs` 模块头）。
//!
//! ── 与 CatPaw / Trae 的形态差异 ──────────────────────────────
//! 同样是 `is_stateful = true`，但 KukuAI 的会话是**请求内**的（建完即用、
//! 用毕即弃），不需要会话注册表 / 轮次状态机：`run_chat` 单函数完成
//! 「建会话 → 流式 → 收尾」，不跨请求保持任何状态。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic；持锁不跨 await。

use serde_json::{json, Map, Value};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ConversationSink, PluginCtx};

use crate::errors::GatewayError;

use super::credentials::{self, KukuCredentials, request_headers};
use super::session::{self, TokenTriple};
use super::{APP_ID, BASE_URL, CHANNEL, DEFAULT_MODEL, DEFAULT_THINK_MODE, WEB_QUERY};

/// 客户端会话通道名（kuku2api 实测值）
const CHANNEL_NAME: &str = "kuku_web_genflowpro_v1";

/// 建会话请求超时
const SESSION_TIMEOUT_MS: u64 = 60_000;

/// 转发入口（有状态路径 `forward_conversation` 转调）。
pub async fn run_chat(
    ctx: &PluginCtx,
    req: proto::ConversationRequest,
    mut sink: ConversationSink<'_>,
) -> Result<(), String> {
    let account_id = req.account.as_str();
    let body = &req.body;
    let proxy = req.proxy.clone();
    let credentials = credentials::snapshot_for(ctx, account_id)
        .await
        .map_err(|error| error.message)?;
    let client_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_MODEL)
        .to_string();
    let model = super::models::resolve_model(&client_model);
    let prompt = build_prompt(body.get("messages"));
    let triple = session::ensure_tokens(&credentials, proxy.as_ref(), false)
        .await
        .map_err(|error| error.message)?;
    let query = format!("{WEB_QUERY}{}", session::token_query(&triple));

    // ── 建会话 ──────────────────────────────────────────────
    let created = create_session(&credentials, &query, &triple, &prompt, &model, proxy.as_ref())
        .await
        .map_err(|error| error.message)?;
    // 分配算力 + 切会话（容错：失败不阻断对话，浏览器也会调这两步）
    if let Err(error) = allocate(&credentials, &query, proxy.as_ref()).await {
        crate::logging::verbose("[Kuku]", &format!("idallochstr 失败（容错）：{}", error.message));
    }
    if let Err(error) = switch_session(&credentials, &query, &created.session_id, proxy.as_ref()).await {
        crate::logging::verbose("[Kuku]", &format!("sessionswitch 失败（容错）：{}", error.message));
    }

    // ── SSE 流 ──────────────────────────────────────────────
    let url = format!(
        "{BASE_URL}/wenchain/genflowpro/sse/getchatcontent?{query}"
    );
    let payload = chat_payload(&prompt, &model, &created.session_id, &created.reply_id);
    let headers = request_headers(&credentials);
    let response = super::http::sse_post(&url, &headers, &payload, proxy.as_ref(), "KukuAI 对话")
        .await
        .map_err(|error| error.message)?;

    // 先报头：网关据此决定状态码与翻译机
    sink.head(proto::StreamHead {
        status: 200,
        headers: super::header_map(&[(
            "content-type".to_string(),
            "text/event-stream".to_string(),
        )]),
        upstream: proto::UpstreamKind::Chat,
    })
    .await?;

    // 逐段推帧。`send` 在背压窗口用完时会挂起 —— 不要自己缓冲整段回答。
    let mut stream = KukuStream::new(&client_model);
    let mut source = crate::response_stream::sse(response);
    use futures::StreamExt;
    while let Some(item) = source.next().await {
        match item {
            Err(error) => {
                push_frames(&mut sink, stream.finish_with_error(&format!(
                    "KukuAI 对话流处理中断：{error}",
                ))).await?;
                return Ok(());
            }
            Ok(chunk) => {
                push_frames(&mut sink, stream.feed(&chunk)).await?;
                if stream.saw_end { return Ok(()); }
            }
        }
    }
    push_frames(&mut sink, stream.finish()).await?;
    Ok(())
}

/// 把一批帧写进 sink。
async fn push_frames(
    sink: &mut ConversationSink<'_>,
    frames: Vec<Frame>,
) -> Result<(), String> {
    for frame in frames {
        sink.send(frame.wire().as_bytes()).await?;
    }
    Ok(())
}

/// 建会话（sendmsg）→ `{session_id, reply_id}`。
///
/// `triple` 是会话三件套：query 里拼一份、`client_added` 里嵌一份
/// （参考实现两处都带真实值，上游据此做客户端形态校验）。
async fn create_session(
    credentials: &KukuCredentials,
    query: &str,
    triple: &TokenTriple,
    prompt: &str,
    model: &str,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
) -> Result<NewSession, GatewayError> {
    let url = format!("{BASE_URL}/wenchain/genflowpro/sendmsg?{query}");
    let headers = request_headers(credentials);
    let csi = random_uuid();
    let now_ms = crate::logging::now_ms();
    let show_text = json!({
        "skills": [], "experts": [], "model_name": model,
        "model_display_name": model, "think_mode": DEFAULT_THINK_MODE,
        "richInputContent": [{"type": "text", "text": prompt}], "fileInfo": []
    });
    let client_added = json!({
        "client_session_id": csi, "permission_type": 0, "clienttype": 400,
        "app_id": APP_ID, "web": 1, "channel": CHANNEL, "version": "1.4.4",
        "bdstoken": triple.bdstoken, "uinfo": triple.uinfo, "uk": triple.uk
    });
    let body = json!({
        "type": "message", "sub_type": "chat_create",
        "client_session_id": csi, "session_id": "", "uk": "", "cid": 0,
        "channel": CHANNEL_NAME, "device_type": 400,
        "created_at": now_ms, "msg_type": 1, "sync_type": 0, "sync_id": "",
        "v": random_hex(24),
        "data": {
            "project_type": 1, "text": prompt,
            "rich_input_params": [{
                "id": "textId", "version": "1.0", "type": "text",
                "text": prompt, "data": {"content": prompt}
            }],
            "fsid": [], "quotes": [], "skills": [], "experts": [],
            "model_name": model, "model_display_name": model,
            "think_mode": DEFAULT_THINK_MODE,
            "show_text": show_text.to_string(),
            "premake_data": {}, "custom_instructions": "",
            "memory_sign": true, "skill_dig_sign": true,
            "client_added": client_added.to_string()
        }
    });
    let value = super::http::post_json_value(&url, &headers, &body, proxy, Some(SESSION_TIMEOUT_MS)).await?;
    let code = value
        .get("status")
        .and_then(|status| status.get("code"))
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    if code != 0 {
        let message = value
            .get("status")
            .and_then(|status| status.get("msg"))
            .and_then(Value::as_str)
            .unwrap_or("未知错误")
            .to_string();
        return Err(GatewayError::with_status(
            502,
            format!("KukuAI 建会话失败（status.code={code}：{message}）"),
        ));
    }
    let info = value.get("data").cloned().unwrap_or(Value::Null);
    let session_id = info
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let reply_id = info
        .get("reply_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if session_id.is_empty() {
        return Err(GatewayError::with_status(
            502,
            "KukuAI 建会话返回缺少 session_id（上游响应异常）",
        ));
    }
    Ok(NewSession { session_id, reply_id })
}

/// 分配算力（容错）。
async fn allocate(
    credentials: &KukuCredentials,
    query: &str,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
) -> Result<(), GatewayError> {
    let url = format!("{BASE_URL}/wenchain/genflow/idallochstr?{query}");
    let headers = request_headers(credentials);
    let body = json!({ "channel": CHANNEL_NAME, "gen_type": 2 });
    super::http::post_json_value(&url, &headers, &body, proxy, Some(SESSION_TIMEOUT_MS)).await?;
    Ok(())
}

/// 切会话（浏览器会调，容错）。
async fn switch_session(
    credentials: &KukuCredentials,
    query: &str,
    session_id: &str,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
) -> Result<(), GatewayError> {
    let url = format!("{BASE_URL}/api/genflowpro/workspace/sessionswitch?{query}");
    let headers = request_headers(credentials);
    let body = json!({ "session_id": session_id, "op": 1, "full_access_enabled": 0 });
    super::http::post_json_value(&url, &headers, &body, proxy, Some(SESSION_TIMEOUT_MS)).await?;
    Ok(())
}

/// 一次建会话的结果。
struct NewSession {
    session_id: String,
    reply_id: String,
}

/// getchatcontent 的请求体（sendmsg 的 `data` 段 + 会话标识）。
fn chat_payload(prompt: &str, model: &str, session_id: &str, reply_id: &str) -> Value {
    let show_text = json!({
        "skills": [], "experts": [], "model_name": model,
        "model_display_name": model, "think_mode": DEFAULT_THINK_MODE,
        "richInputContent": [{"type": "text", "text": prompt}], "fileInfo": []
    });
    json!({
        "project_type": 1, "text": prompt,
        "rich_input_params": [{
            "id": "textId", "version": "1.0", "type": "text",
            "text": prompt, "data": {"content": prompt}
        }],
        "fsid": [], "quotes": [], "skills": [], "experts": [],
        "model_name": model, "model_display_name": model,
        "think_mode": DEFAULT_THINK_MODE,
        "show_text": show_text.to_string(),
        "premake_data": {}, "custom_instructions": "",
        "memory_sign": true, "skill_dig_sign": true,
        "session_id": session_id, "reply_id": reply_id
    })
}

/// 把 OpenAI messages 压成单条 prompt（上游无多轮接续接口，参考实现同口径）。
pub fn build_prompt(messages: Option<&Value>) -> String {
    let Some(Value::Array(items)) = messages else {
        return "你好".to_string();
    };
    if items.is_empty() {
        return "你好".to_string();
    }
    if items.len() == 1 {
        return content_text(&items[0]).unwrap_or_else(|| "你好".to_string());
    }
    let mut lines: Vec<String> = Vec::new();
    for message in items {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("user");
        let text = content_text(message);
        let Some(text) = text else { continue };
        if text.trim().is_empty() {
            continue;
        }
        match role {
            "system" => lines.push(format!("[系统指令] {text}")),
            "assistant" => lines.push(format!("[助手之前的回复] {text}")),
            _ => lines.push(text),
        }
    }
    if lines.is_empty() {
        return "你好".to_string();
    }
    lines.join("\n\n")
}

/// 取一条消息的可读文本（content 字符串或 text 类型的部件数组）。
fn content_text(message: &Value) -> Option<String> {
    let content = message.get("content")?;
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let mut out: Vec<String> = Vec::new();
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    out.push(text.to_string());
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(out.join("\n"))
            }
        }
        _ => None,
    }
}

// ─── SSE 解析 ────────────────────────────────────────────────

/// 一条已解析的上游事件。
enum KukuEvent {
    Delta(String),
    Done,
    Error(String),
}

/// 按完整 SSE 事件解析，跨块保留原始字节。
#[derive(Default)]
struct SseScanner {
    events: crate::sse_events::DataEvents,
}

impl SseScanner {
    fn push(&mut self, bytes: &[u8]) -> Vec<KukuEvent> {
        self.events.push(bytes).into_iter().filter_map(|payload| {
            if crate::sse_events::is_done(payload.as_bytes()) { return Some(KukuEvent::Done); }
            serde_json::from_str::<Value>(&payload).ok().as_ref().and_then(event_from_json)
        }).collect()
    }

    fn finish(&mut self) -> Vec<KukuEvent> {
        self.events.finish().into_iter().filter_map(|payload| {
            serde_json::from_str::<Value>(&payload).ok().as_ref().and_then(event_from_json)
                .filter(|event| !matches!(event, KukuEvent::Done))
        }).collect()
    }
}

/// 一条 `data:` JSON → 事件（认不得的 type 忽略，与参考实现一致）。
fn event_from_json(parsed: &Value) -> Option<KukuEvent> {
    match parsed.get("type").and_then(Value::as_str) {
        Some("TEXT_BLOCK_DELTA") => {
            let delta = parsed
                .get("data")
                .and_then(|data| data.get("delta"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if delta.is_empty() {
                None
            } else {
                Some(KukuEvent::Delta(delta))
            }
        }
        Some("REPLY_END") | Some("DIALOGUE_END") => Some(KukuEvent::Done),
        Some("ERROR") => {
            let message = parsed
                .get("data")
                .and_then(|data| data.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("上游错误")
                .to_string();
            Some(KukuEvent::Error(message))
        }
        _ => None,
    }
}

// ─── OpenAI chunk 产出 ──────────────────────────────────────

/// 一帧出站内容。
enum Frame {
    Chunk(Value),
    Error(String),
    Done,
}

impl Frame {
    /// 落到线上的字节（含 `\n\n` 帧尾）。
    fn wire(&self) -> String {
        match self {
            Frame::Chunk(chunk) => format!("data: {chunk}\n\n"),
            Frame::Error(message) => format!("data: {}\n\n", json!({
                "error": { "message": message, "type": "upstream_error" },
            })),
            Frame::Done => "data: [DONE]\n\n".to_string(),
        }
    }
}

/// 流式转换器：喂上游字节，产出 OpenAI chunk。
struct KukuStream {
    id: String,
    created: i64,
    model: String,
    scanner: SseScanner,
    saw_end: bool,
    /// 非流式聚合用（也用于流式路径的错误兜底判断：发过内容就不再改判 HTTP 错误）
    content: String,
}

impl KukuStream {
    fn new(model: &str) -> Self {
        Self {
            id: format!("chatcmpl-{}", crate::logging::now_ms() * 1000 % 1_000_000_000_000_000),
            created: crate::logging::now_ms() / 1000,
            model: model.to_string(),
            scanner: SseScanner::default(),
            saw_end: false,
            content: String::new(),
        }
    }

    /// 喂一段字节，返回该段触发的帧。
    fn feed(&mut self, bytes: &[u8]) -> Vec<Frame> {
        let events = self.scanner.push(bytes);
        self.consume(events)
    }

    fn consume(&mut self, events: Vec<KukuEvent>) -> Vec<Frame> {
        if self.saw_end { return Vec::new(); }
        let mut frames = Vec::new();
        for event in events {
            match event {
                KukuEvent::Delta(delta) => {
                    self.content.push_str(&delta);
                    frames.push(self.chunk(delta, None));
                }
                KukuEvent::Done => {
                    self.saw_end = true;
                    frames.push(self.chunk(String::new(), Some("stop")));
                    frames.push(Frame::Done);
                    break;
                }
                KukuEvent::Error(message) => {
                    self.saw_end = true;
                    frames.push(Frame::Error(message));
                    frames.push(Frame::Done);
                    break;
                }
            }
        }
        frames
    }

    fn flush_tail(&mut self) -> Vec<Frame> {
        let events = self.scanner.finish();
        self.consume(events)
    }

    fn finish(&mut self) -> Vec<Frame> {
        let mut frames = self.flush_tail();
        if !self.saw_end { frames.push(Frame::Done); }
        self.saw_end = true;
        frames
    }

    fn finish_with_error(&mut self, message: &str) -> Vec<Frame> {
        let mut frames = self.flush_tail();
        if !self.saw_end {
            frames.push(Frame::Error(message.to_string()));
            frames.push(Frame::Done);
        }
        self.saw_end = true;
        frames
    }

    fn chunk(&mut self, delta: String, finish_reason: Option<&str>) -> Frame {
        let mut choice = Map::new();
        choice.insert("index".to_string(), json!(0));
        if delta.is_empty() {
            choice.insert("delta".to_string(), json!({}));
        } else {
            choice.insert("delta".to_string(), json!({ "content": delta }));
        }
        if let Some(reason) = finish_reason {
            choice.insert("finish_reason".to_string(), json!(reason));
        }
        let mut chunk = Map::new();
        chunk.insert("id".to_string(), json!(self.id));
        chunk.insert("object".to_string(), json!("chat.completion.chunk"));
        chunk.insert("created".to_string(), json!(self.created));
        chunk.insert("model".to_string(), json!(self.model));
        chunk.insert("choices".to_string(), Value::Array(vec![Value::Object(choice)]));
        Frame::Chunk(Value::Object(chunk))
    }
}

// ─── 流式 / 非流式驱动 ──────────────────────────────────────

// ─── 随机串 ──────────────────────────────────────────────────

/// UUID v4 形态（`client_session_id` 用）。
fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::getrandom(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// 随机十六进制串（`v` 字段用，参考实现 24 位）。
fn random_hex(length: usize) -> String {
    let mut out = String::new();
    while out.len() < length {
        let mut bytes = [0u8; 16];
        let _ = getrandom::getrandom(&mut bytes);
        for byte in bytes {
            out.push_str(&format!("{byte:02x}"));
            if out.len() >= length {
                break;
            }
        }
    }
    out
}
