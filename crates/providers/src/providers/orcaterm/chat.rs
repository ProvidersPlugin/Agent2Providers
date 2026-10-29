//! OrcaTerm 对话：建会话 → assistant SSE → OpenAI 帧。
//!
//! ── 上游是两步（2026-10-16 实测，curl 直连复现）──────────────
//!   1. `POST {LIGHTAI}/assistant/conversation` → `cid-<uuid>-orcaterm`；
//!   2. `POST {LIGHTAI}/assistant/chat?name=orcaterm&mode=0&csrfCode=&sseresume=true`
//!      → SSE：`data: {content|reasoningContent}` … `data: [DONE]`。
//! SSE 帧字段：`content` / `reasoningContent` / `reasoning`（true = 思考增量）
//! / `type:"ai"` / `code:0`；错误帧**不是 JSON**：`data: [ERROR]: [LLM_ERROR]: …`。
//!
//! ── 多轮：历史拍平为单条输入（与 KukuAI 同一口径）────────────
//! 上游按 conversationId 在服务端留存历史，但输入通道只有一条 `start`
//! 消息，而 OpenAI 客户端每轮自带全量 messages —— 因此按**请求内会话**
//! 处理：每个请求新建会话，历史拍平进输入文本。无注册表、无指纹链、
//! 重启即忘。
//!
//! ── 信封解包（本家独有的下游形态）───────────────────────────
//! 上游是 agent 协议：模型正文倾向输出 ACP 动作信封（```json 围栏或裸
//! JSON，形如 `{"action":"completion","taskCompletion":"…"}`），桌面端 UI
//! 自己解析它 —— 但 OpenAI 客户端要的是纯文本。因此第一个正文增量到达时
//! 判定形态（``` / `{` 开头 → 缓冲模式），结束时解包；解不出就原样透传。
//!
//! ── 401 自愈 ───────────────────────────────────────────────
//! 建会话 / 对话拿到 401 时强制续期一次并重发同一步（只重试一次）。

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ConversationSink, PluginCtx};
use serde_json::{json, Map, Value};

use super::credentials::{self, Credentials};
use super::http::{self, HttpFailure};
use super::{models, ASSISTANT_QUERY, LIGHTAI_BASE};

/// 一次 401 自愈重试的上限（每步各一次，不循环）。
const AUTH_RETRY: usize = 1;

/// 转发入口。
pub async fn run(
    ctx: &PluginCtx,
    req: proto::ConversationRequest,
    mut sink: ConversationSink<'_>,
) -> Result<(), String> {
    let control_client = crate::http::control_client(req.proxy.as_ref(), &ctx.env.proxy);
    let mut credentials = credentials::snapshot(ctx, &req.account).await?;
    // 临期主动续期（结果回写账号记录；单飞保证并发只刷一次）
    credentials = credentials::refresh(ctx, &credentials, &req.account, false).await?;

    let client_model = req.body.get("model").and_then(Value::as_str).unwrap_or("").to_string();
    let model = models::resolve_model(&client_model);
    let prompt = build_prompt(req.body.get("messages"));

    // ── 建会话（401 自愈：强制续期后重试一次）────────────────
    let mut conversation = create_conversation(&control_client, &credentials).await;
    let mut attempts = 0;
    while let Err(error) = &conversation {
        if !(error.is_auth() && attempts < AUTH_RETRY) {
            break;
        }
        if !refresh_for_retry(ctx, &mut credentials, &req.account).await {
            break;
        }
        attempts += 1;
        conversation = create_conversation(&control_client, &credentials).await;
    }
    let conversation = conversation.map_err(|error| error.message)?;

    // ── 开流（同样 401 自愈）──────────────────────────────────
    let response = match open_chat_stream(req.proxy.as_ref(), &ctx.env.proxy, &credentials, &conversation, &model, &prompt).await
    {
        Ok(response) => response,
        Err(error) => {
            if !error.is_auth() || !refresh_for_retry(ctx, &mut credentials, &req.account).await {
                return Err(error.message);
            }
            // 会话可能已在上一步建出（无妨：换 token 后重开流，conversationId 仍有效）
            open_chat_stream(req.proxy.as_ref(), &ctx.env.proxy, &credentials, &conversation, &model, &prompt)
                .await
                .map_err(|error| error.message)?
        }
    };

    // 先报头：网关据此决定状态码与翻译机
    sink.head(proto::StreamHead {
        status: 200,
        headers: super::header_map(&[("content-type".to_string(), "text/event-stream".to_string())]),
        upstream: proto::UpstreamKind::Chat,
    })
    .await?;

    // 逐段推帧。`send` 在背压窗口用完时会挂起 —— 不要自己缓冲整段回答。
    let mut stream = OrcaStream::new(&client_model);
    let mut source = crate::response_stream::sse(response);
    use futures::StreamExt;
    while let Some(item) = source.next().await {
        match item {
            Err(error) => {
                let frames = stream.finish_with_error(&format!("OrcaTerm 上游流式传输中断: {error}"));
                push_frames(&mut sink, frames).await?;
                return Ok(());
            }
            Ok(chunk) => {
                push_frames(&mut sink, stream.feed(&chunk)).await?;
                if stream.saw_end { return Ok(()); }
            }
        }
    }
    push_frames(&mut sink, stream.finish()).await
}

/// 把一组帧推给网关。
async fn push_frames(sink: &mut ConversationSink<'_>, frames: Vec<Frame>) -> Result<(), String> {
    for frame in frames {
        sink.send(&frame.wire()).await?;
    }
    Ok(())
}

/// 401 一步的自愈动作：强制续期；成功返回 true（调用方重发同一步）。
async fn refresh_for_retry(ctx: &PluginCtx, credentials: &mut Credentials, account: &str) -> bool {
    match credentials::refresh(ctx, credentials, account, true).await {
        Ok(next) => {
            *credentials = next;
            true
        }
        Err(_) => false,
    }
}

/// 建会话 → `cid-<uuid>-orcaterm`。
async fn create_conversation(
    client: &reqwest::Client,
    credentials: &Credentials,
) -> Result<String, HttpFailure> {
    let uuid = random_uuid();
    let url = format!("{LIGHTAI_BASE}/assistant/conversation");
    let body = json!({
        "id": uuid,
        "title": "新对话",
        "ts": crate::http::now_ms(),
        "customTitle": "",
        "aiTitle": "",
        "tags": ["orcaterm"],
    });
    let value = http::post_json(
        client,
        &url,
        &credentials::lightai_headers(credentials, &random_uuid()),
        &body,
    )
    .await?;
    let code = value.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code != 0 {
        let message = value.get("message").and_then(Value::as_str).unwrap_or("未知错误");
        return Err(HttpFailure {
            status: 502,
            message: format!("OrcaTerm 建会话失败（code={code}：{message}）"),
        });
    }
    Ok(format!("cid-{uuid}-orcaterm"))
}

/// 打开对话 SSE 流。
async fn open_chat_stream(
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
    credentials: &Credentials,
    conversation_id: &str,
    model: &str,
    prompt: &str,
) -> Result<reqwest::Response, HttpFailure> {
    let seq_id = random_uuid();
    let url = format!("{LIGHTAI_BASE}/assistant/chat?{ASSISTANT_QUERY}");
    let body = json!({
        "conversationId": conversation_id,
        "user": {
            "id": credentials.user_id,
            "setting": {
                "model": model,
                "mcpServers": [],
                "uiServers": [],
                "tools": [],
                "approvedTools": [],
                "approvedMCPTools": [],
                "enableAutoSubtaskExecution": false,
                "env": [],
                // 上游是 agent 协议；网关不做工具注入（纯文本直通）
                "hideToolMessage": true,
                "language": "zh",
            },
        },
        "input": {
            "type": "start",
            "input": prompt,
            "id": seq_id,
            "emphasisPrompt": "",
            "files": [],
        },
        "stream": true,
    });
    let client = crate::http::stream_client(proxy, gateway_proxy);
    http::sse_post(
        &client,
        &url,
        &credentials::lightai_headers(credentials, &seq_id),
        &body,
        "OrcaTerm 对话",
    )
    .await
}

/// 把 OpenAI messages 压成单条输入。
///
/// 单条消息原样直通（最常见的单轮用法不引入任何包装）；多轮时把历史拍平成
/// 带角色前缀的记录，让上游把整段当作对话上下文来回应最后一条用户消息。
pub fn build_prompt(messages: Option<&Value>) -> String {
    let Some(Value::Array(items)) = messages else {
        return "你好".to_string();
    };
    let usable: Vec<&Value> = items
        .iter()
        .filter(|message| {
            content_text(message).map(|text| !text.trim().is_empty()).unwrap_or(false)
        })
        .collect();
    if usable.is_empty() {
        return "你好".to_string();
    }
    if usable.len() == 1 {
        return content_text(usable[0]).unwrap_or_else(|| "你好".to_string());
    }
    let mut lines: Vec<String> = Vec::new();
    lines.push("[以下是本轮之前的对话记录，供你了解上下文]".to_string());
    for message in usable {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("user");
        let Some(text) = content_text(message) else { continue };
        match role {
            "system" => lines.push(format!("[系统指令] {text}")),
            "assistant" => lines.push(format!("[助手] {text}")),
            _ => lines.push(format!("[用户] {text}")),
        }
    }
    lines.push("[对话记录结束；请回应其中最后一条用户消息]".to_string());
    lines.join("\n\n")
}

/// 取一条消息的可读文本（content 字符串或 text 类型部件数组）。
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
enum OrcaEvent {
    /// 思考增量（`reasoning:true` 帧）
    Reasoning(String),
    /// 正文增量（`reasoning:false` 帧的 `content`）
    Delta(String),
    /// 结束（`data: [DONE]`）
    Done,
    /// 流内错误（`data: [ERROR]: …`；HTTP 恒 200，错误在流里）
    Error(String),
}

/// 完整 SSE 事件解码，正文内的 DONE 字符串不是终止标记。
#[derive(Default)]
struct Scanner {
    events: crate::sse_events::DataEvents,
}

impl Scanner {
    fn push(&mut self, bytes: &[u8]) -> Vec<OrcaEvent> {
        self.events.push(bytes).into_iter().filter_map(|payload| parse_event(&payload)).collect()
    }

    fn finish(&mut self) -> Vec<OrcaEvent> {
        self.events.finish().into_iter().filter_map(|payload| parse_event(&payload)).collect()
    }
}

fn parse_event(payload: &str) -> Option<OrcaEvent> {
    if crate::sse_events::is_done(payload.as_bytes()) { return Some(OrcaEvent::Done); }
    if let Some(error) = payload.trim_start().strip_prefix("[ERROR]:") {
        return Some(OrcaEvent::Error(error.trim().to_string()));
    }
    let parsed: Value = serde_json::from_str(payload).ok()?;
    if parsed.get("type").and_then(Value::as_str) != Some("ai") { return None; }
    let reasoning = parsed.get("reasoning").and_then(Value::as_bool).unwrap_or(false);
    let key = if reasoning { "reasoningContent" } else { "content" };
    let text = parsed.get(key).and_then(Value::as_str).filter(|text| !text.is_empty())?.to_string();
    Some(if reasoning { OrcaEvent::Reasoning(text) } else { OrcaEvent::Delta(text) })
}

// ─── OpenAI 帧产出 ──────────────────────────────────────────

/// 一帧出站内容。
enum Frame {
    Chunk(Value),
    /// OpenAI 形态的流内错误帧 —— 整程没有正文、只有上游错误时下发，
    /// 客户端据此显示失败原因而不是空白。
    Error(String),
    Done,
}

impl Frame {
    /// 落到线上的字节（含 `\n\n` 帧尾）。
    fn wire(&self) -> Vec<u8> {
        match self {
            Frame::Chunk(chunk) => crate::sse::sse_bytes(chunk),
            Frame::Error(message) => crate::sse::error_bytes(message),
            Frame::Done => crate::sse::done_bytes(),
        }
    }
}

/// 流式转换器：喂上游字节，产出 OpenAI chunk。
struct OrcaStream {
    id: String,
    created: i64,
    model: String,
    scanner: Scanner,
    saw_end: bool,
    /// 正文缓冲（信封模式下累积；直播模式恒空）
    content: String,
    /// 形态判定结果（第一个正文增量到达时定档）
    buffered: Option<bool>,
    /// 最后一条流内错误（正文为空时它就是这次请求的真实结果）
    last_error: Option<String>,
}

impl OrcaStream {
    fn new(model: &str) -> Self {
        let now = crate::http::now_ms();
        Self {
            id: format!("chatcmpl-{}", now * 1000 % 1_000_000_000_000_000),
            created: now / 1000,
            model: model.to_string(),
            scanner: Scanner::default(),
            saw_end: false,
            content: String::new(),
            buffered: None,
            last_error: None,
        }
    }

    /// 喂一段字节，返回该段触发的帧。
    fn feed(&mut self, bytes: &[u8]) -> Vec<Frame> {
        let events = self.scanner.push(bytes);
        self.consume(events)
    }

    fn consume(&mut self, events: Vec<OrcaEvent>) -> Vec<Frame> {
        if self.saw_end { return Vec::new(); }
        let mut frames = Vec::new();
        for event in events {
            match event {
                OrcaEvent::Reasoning(delta) => {
                    frames.push(self.chunk(delta, None, true));
                }
                OrcaEvent::Delta(delta) => match self.buffered {
                    // 形态未定：看开头是不是信封形态（``` 围栏 / JSON 对象）
                    None => {
                        if self.content.is_empty() && !delta.trim_start().is_empty() {
                            let head = delta.trim_start();
                            let envelope_shaped =
                                head.starts_with("```") || head.starts_with('{');
                            self.buffered = Some(envelope_shaped);
                        }
                        self.content.push_str(&delta);
                        if !self.buffered.unwrap_or(false) {
                            frames.push(self.chunk(delta, None, false));
                        }
                    }
                    // 信封形态：只缓冲，不发帧
                    Some(true) => self.content.push_str(&delta),
                    // 直播形态：逐增量下发
                    Some(false) => frames.push(self.chunk(delta, None, false)),
                },
                OrcaEvent::Done => {
                    self.saw_end = true;
                    frames.extend(self.final_frames(Some("stop")));
                    break;
                }
                OrcaEvent::Error(message) => {
                    self.last_error = Some(message);
                    self.saw_end = true;
                    frames.extend(self.final_frames(None));
                    break;
                }
            }
        }
        frames
    }

    /// 收尾帧：信封模式在这里解包并一次性下发正文。
    ///
    /// ── 流内错误的两种处置 ──────────────────────────────────
    /// 正文**为空**时，流内错误就是整次请求的结果 —— 下发一帧 error 对象
    /// 再收尾（面板上那个未挂载的模型走的就是这条路）；已发出正文后的
    /// ERROR 仍保留内容、静默收尾。
    fn final_frames(&mut self, finish_reason: Option<&str>) -> Vec<Frame> {
        let mut frames = Vec::new();
        if self.buffered == Some(true) {
            let raw = self.content.clone();
            match unwrap_envelope(&raw) {
                Some(text) => frames.push(self.chunk(text, finish_reason, false)),
                // 解不出信封文本 = 不是信封（或字段认不出）：原样下发，
                // 宁可透传也不错杀用户自己要的 JSON
                None => {
                    if !raw.is_empty() {
                        frames.push(self.chunk(raw, finish_reason, false));
                    } else if let Some(reason) = finish_reason {
                        frames.push(self.chunk(String::new(), Some(reason), false));
                    }
                }
            }
        } else if let Some(reason) = finish_reason {
            // 直播模式：内容已实时下发，只补结束帧
            frames.push(self.chunk(String::new(), Some(reason), false));
        }
        if self.content.is_empty() {
            if let Some(message) = &self.last_error {
                frames.push(Frame::Error(message.clone()));
            }
        }
        frames.push(Frame::Done);
        frames
    }

    fn flush_tail(&mut self) -> Vec<Frame> {
        let events = self.scanner.finish();
        self.consume(events)
    }

    fn finish(&mut self) -> Vec<Frame> {
        let mut frames = self.flush_tail();
        if !self.saw_end { frames.extend(self.final_frames(Some("stop"))); }
        self.saw_end = true;
        frames
    }

    fn finish_with_error(&mut self, message: &str) -> Vec<Frame> {
        let mut frames = self.flush_tail();
        if self.saw_end { return frames; }
        self.saw_end = true;
        let mut tail = self.final_frames(None);
        if !tail.iter().any(|frame| matches!(frame, Frame::Error(_))) {
            tail.insert(tail.len().saturating_sub(1), Frame::Error(message.to_string()));
        }
        frames.extend(tail);
        frames
    }

    fn chunk(&mut self, delta: String, finish_reason: Option<&str>, reasoning: bool) -> Frame {
        let mut delta_body = Map::new();
        if !delta.is_empty() {
            if reasoning {
                delta_body.insert("reasoning_content".to_string(), json!(delta));
            } else {
                delta_body.insert("content".to_string(), json!(delta));
            }
        }
        Frame::Chunk(crate::sse::chat_chunk(
            &self.id,
            &self.model,
            self.created,
            Value::Object(delta_body),
            finish_reason,
        ))
    }
}

/// ACP 动作信封解包：整段正文（剥一层 ``` 围栏后）是带 `action` 键的 JSON
/// 对象时，抽出给人看的文本。
///
/// 取值链照抄桌面端 UI：`completion → taskCompletion`、`default → content`、
/// 其余动作按 `taskCompletion / content / question` 取第一个非空。
fn unwrap_envelope(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let inner = if trimmed.starts_with("```") {
        trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim()
    } else {
        trimmed
    };
    let parsed: Value = serde_json::from_str(inner).ok()?;
    let object = parsed.as_object()?;
    let action = object.get("action").and_then(Value::as_str)?;
    let text = match action {
        "completion" => object.get("taskCompletion").and_then(Value::as_str),
        "default" => object.get("content").and_then(Value::as_str),
        _ => ["taskCompletion", "content", "question"]
            .iter()
            .find_map(|key| object.get(*key).and_then(Value::as_str)),
    }?
    .trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// UUID v4 形态（会话 id / x-seqid 用）。
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
