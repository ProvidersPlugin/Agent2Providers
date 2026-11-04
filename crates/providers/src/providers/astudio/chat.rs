//! AStudio 的会话式转发：Responses 请求 → 标准 chat SSE。
//!
//! ── 为什么走 `conversation` ────────────────────────────────
//! 上游说 OpenAI Responses，网关的 `UpstreamKind` 只有 Chat / Anthropic /
//! Ndjson / Gemini / 混合快照。插件自己读 Responses 的 SSE 事件、翻成 chat
//! 帧，就不用扩协议；账号轮换、冷却、统计仍由网关负责。
//!
//! ── 非流式请求也按流式发 ────────────────────────────────────
//! 上游的 Responses 流是逐事件下发的；插件统一推 SSE 帧，非流式请求由网关
//! 侧聚合（与豆包工作等有状态家同一处置）。

use std::collections::BTreeMap;

use bytes::Bytes;
use futures::{stream::BoxStream, StreamExt};
use serde_json::{json, Value};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ConversationSink, PluginCtx};

use super::accounts::Credentials;
use super::client;
use super::responses::{self, Piece};
use super::PROVIDER_ID;
use crate::sse::{LineBuffer, SseLine};
use crate::{http, ids, logging};

fn sse(payload: &Value) -> Vec<u8> {
    format!("data: {payload}\n\n").into_bytes()
}

fn chunk(model: &str, id: &str, delta: Value, finish: Option<&str>) -> Vec<u8> {
    sse(&json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
    }))
}

fn usage_chunk(
    model: &str,
    id: &str,
    reason: &str,
    input: i64,
    output: i64,
    cached: i64,
) -> Vec<u8> {
    sse(&json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{ "index": 0, "delta": {}, "finish_reason": reason }],
        "usage": {
            "prompt_tokens": input.max(0),
            "completion_tokens": output.max(0),
            "total_tokens": (input + output).max(0),
            "prompt_tokens_details": { "cached_tokens": cached.max(0) },
        },
    }))
}

async fn head(sink: &mut ConversationSink<'_>, status: u16) -> Result<(), String> {
    sink.head(proto::StreamHead {
        status,
        headers: BTreeMap::from([(
            "content-type".to_string(),
            if status == 200 {
                "text/event-stream"
            } else {
                "application/json"
            }
            .to_string(),
        )]),
        upstream: proto::UpstreamKind::Chat,
    })
    .await
}

/// Responses 的 SSE 事件读取器。
struct EventStream {
    stream: BoxStream<'static, Result<Bytes, reqwest::Error>>,
    lines: LineBuffer,
    pending: std::collections::VecDeque<Value>,
    ended: bool,
}

impl EventStream {
    fn new(response: reqwest::Response) -> Self {
        Self {
            stream: Box::pin(response.bytes_stream()),
            lines: LineBuffer::new(),
            pending: std::collections::VecDeque::new(),
            ended: false,
        }
    }

    async fn next(&mut self) -> Result<Option<Value>, String> {
        loop {
            if let Some(value) = self.pending.pop_front() {
                return Ok(Some(value));
            }
            if self.ended {
                return Ok(None);
            }
            match self.stream.next().await {
                Some(Ok(bytes)) => {
                    for line in self.lines.push(&bytes) {
                        self.handle(&line);
                    }
                }
                Some(Err(error)) => {
                    return Err(format!(
                        "读取 AStudio 的响应失败：{}",
                        http::describe(&error)
                    ))
                }
                None => {
                    self.ended = true;
                    if let Some(line) = self.lines.finish() {
                        self.handle(&line);
                    }
                }
            }
        }
    }

    fn handle(&mut self, line: &str) {
        let parsed = SseLine::parse(line);
        let Some(payload) = parsed.data() else {
            return;
        };
        if payload == "[DONE]" {
            self.ended = true;
            return;
        }
        if let Ok(value) = serde_json::from_str::<Value>(payload) {
            self.pending.push_back(value);
        }
    }
}

/// 一次完整转发。
pub async fn run(
    ctx: &PluginCtx,
    request: proto::ConversationRequest,
    mut sink: ConversationSink<'_>,
) -> Result<(), String> {
    match forward(ctx, &request, &mut sink).await {
        Ok(()) => Ok(()),
        Err((status, message)) => {
            logging::verbose("astudio", &format!("转发失败（{status}）：{message}"));
            head(&mut sink, status).await?;
            let body = client::error_body(status, &message).to_string();
            sink.send(body.as_bytes()).await
        }
    }
}

async fn forward(
    ctx: &PluginCtx,
    request: &proto::ConversationRequest,
    sink: &mut ConversationSink<'_>,
) -> Result<(), (u16, String)> {
    let stored = ctx
        .store_get(PROVIDER_ID, &request.account)
        .await
        .map_err(|error| (502, format!("读取 AStudio 账号失败：{error}")))?;
    let credentials = Credentials::from_secret(&stored.secret);
    if credentials.key.is_empty() {
        return Err((
            401,
            "AStudio 账号缺少 modelBearerToken，请重新登录或粘贴".to_string(),
        ));
    }
    let model = if request.wire_model.trim().is_empty() {
        request
            .body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("astronclaw-auto")
            .to_string()
    } else {
        request.wire_model.clone()
    };
    let body = responses::build_request(&request.body, &model, true);
    let response =
        client::responses(&credentials, &body, request.proxy.as_ref(), &ctx.env.proxy).await?;
    let mut stream = EventStream::new(response);
    let id = format!("chatcmpl-{}", ids::new_message_id());

    // 预读：跳过 response.created 之类的元事件，第一个有内容的事件决定成败。
    let mut tool_index: i64 = -1;
    let mut tool_args_sent = false;
    let mut text_sent = false;
    let mut started = false;
    let mut usage = (0i64, 0i64, 0i64);
    loop {
        let event = match stream.next().await {
            Ok(Some(event)) => event,
            Ok(None) => {
                if !started {
                    return Err((502, "AStudio 在开始回答前就结束了响应".to_string()));
                }
                sink.send(&usage_chunk(&model, &id, "stop", usage.0, usage.1, usage.2))
                    .await
                    .map_err(|message| (502, message))?;
                sink.send(b"data: [DONE]\n\n")
                    .await
                    .map_err(|message| (502, message))?;
                return Ok(());
            }
            Err(message) => return Err((502, message)),
        };
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "response.created" || kind == "response.in_progress" {
            continue;
        }
        if kind == "error" || kind == "response.failed" {
            let message = event
                .pointer("/response/error/message")
                .and_then(Value::as_str)
                .or_else(|| event.get("message").and_then(Value::as_str))
                .or_else(|| event.pointer("/error/message").and_then(Value::as_str))
                .unwrap_or("AStudio 返回了一个错误")
                .to_string();
            let status = classify_message(&message);
            if !started {
                return Err((status, message));
            }
            sink.send(&sse(&client::error_body(status, &message)))
                .await
                .map_err(|error| (502, error))?;
            sink.send(b"data: [DONE]\n\n")
                .await
                .map_err(|error| (502, error))?;
            return Ok(());
        }
        if kind == "response.completed" || kind == "response.done" || kind == "response.incomplete"
        {
            let (input, output, cached) = responses::usage_of(&event);
            usage = (input, output, cached);
            let reason = if kind == "response.incomplete" {
                "length"
            } else {
                "stop"
            };
            if !started {
                head(sink, 200).await.map_err(|message| (502, message))?;
                sink.send(&chunk(
                    &model,
                    &id,
                    json!({ "role": "assistant", "content": "" }),
                    None,
                ))
                .await
                .map_err(|message| (502, message))?;
            }
            sink.send(&usage_chunk(&model, &id, reason, usage.0, usage.1, usage.2))
                .await
                .map_err(|message| (502, message))?;
            sink.send(b"data: [DONE]\n\n")
                .await
                .map_err(|message| (502, message))?;
            return Ok(());
        }
        if !started {
            head(sink, 200).await.map_err(|message| (502, message))?;
            sink.send(&chunk(
                &model,
                &id,
                json!({ "role": "assistant", "content": "" }),
                None,
            ))
            .await
            .map_err(|message| (502, message))?;
            started = true;
        }
        let mut pending = Some(event);
        while let Some(event) = pending.take() {
            let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
            match kind {
                "response.output_item.added" => {
                    let item = event.get("item").cloned().unwrap_or(Value::Null);
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        let call_id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .or_else(|| item.get("id").and_then(Value::as_str))
                            .unwrap_or("")
                            .to_string();
                        let name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        tool_index += 1;
                        tool_args_sent = false;
                        sink.send(&chunk(
                            &model,
                            &id,
                            json!({
                                "tool_calls": [{
                                    "index": tool_index,
                                    "id": call_id,
                                    "type": "function",
                                    "function": { "name": name, "arguments": "" },
                                }]
                            }),
                            None,
                        ))
                        .await
                        .map_err(|message| (502, message))?;
                    }
                }
                "response.output_text.delta" | "response.text.delta" => {
                    if let Some(text) = event.get("delta").and_then(Value::as_str) {
                        if !text.is_empty() {
                            text_sent = true;
                            sink.send(&chunk(&model, &id, json!({ "content": text }), None))
                                .await
                                .map_err(|message| (502, message))?;
                        }
                    }
                }
                "response.output_text.done" | "response.text.done" => {
                    // 只有没收到增量时才用整段（避免重复）。
                    if !text_sent {
                        if let Some(text) = event.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                text_sent = true;
                                sink.send(&chunk(&model, &id, json!({ "content": text }), None))
                                    .await
                                    .map_err(|message| (502, message))?;
                            }
                        }
                    }
                }
                "response.reasoning_summary_text.delta"
                | "response.reasoning_text.delta"
                | "response.reasoning.delta" => {
                    if let Some(text) = event.get("delta").and_then(Value::as_str) {
                        if !text.is_empty() {
                            sink.send(&chunk(
                                &model,
                                &id,
                                json!({ "reasoning_content": text }),
                                None,
                            ))
                            .await
                            .map_err(|message| (502, message))?;
                        }
                    }
                }
                "response.function_call_arguments.delta" => {
                    if let Some(args) = event.get("delta").and_then(Value::as_str) {
                        if !args.is_empty() && tool_index >= 0 {
                            tool_args_sent = true;
                            sink.send(&chunk(
                                &model,
                                &id,
                                json!({
                                    "tool_calls": [{
                                        "index": tool_index,
                                        "function": { "arguments": args },
                                    }]
                                }),
                                None,
                            ))
                            .await
                            .map_err(|message| (502, message))?;
                        }
                    }
                }
                "response.function_call_arguments.done" => {
                    // 有些实现只在 done 里给完整参数；增量没来过时补一次。
                    if !tool_args_sent && tool_index >= 0 {
                        if let Some(args) = event.get("arguments").and_then(Value::as_str) {
                            if !args.is_empty() {
                                tool_args_sent = true;
                                sink.send(&chunk(
                                    &model,
                                    &id,
                                    json!({
                                        "tool_calls": [{
                                            "index": tool_index,
                                            "function": { "arguments": args },
                                        }]
                                    }),
                                    None,
                                ))
                                .await
                                .map_err(|message| (502, message))?;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// 错误文案 → HTTP 状态（尽力而为）。
fn classify_message(message: &str) -> u16 {
    let lower = message.to_ascii_lowercase();
    if lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("quota")
        || lower.contains("balance")
        || lower.contains("credit")
        || lower.contains("积分")
        || lower.contains("额度")
    {
        return 429;
    }
    if lower.contains("context")
        || lower.contains("too long")
        || lower.contains("maximum length")
        || lower.contains("too large")
    {
        return 400;
    }
    502
}

/// 非流式回退用：把完整响应翻成 pieces（当前只用于调试与测试）。
#[allow(dead_code)]
pub fn pieces(response: &Value) -> Vec<Piece> {
    responses::pieces_of(response)
}
