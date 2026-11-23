//! Kiro 的会话式转发：发一次 `generateAssistantResponse`，把事件流翻成
//! OpenAI chat SSE。
//!
//! ── 为什么必须是会话式（`stateful = true`）────────────────────
//! 上游响应是 AWS event stream，网关的 `UpstreamKind` 没有这个变体。走
//! `conversation` 之后，插件自己读帧、自己推标准 chat 帧，网关的账号轮换、
//! 冷却与统计照旧工作。
//!
//! ── 先读第一帧再决定状态码 ────────────────────────────────
//! 上游经常先回 200、再把错误放进流里（`throttlingException` 等）。如果直接
//! 推 200，网关与客户端都会当成一次成功回答。所以这里**预读第一个事件**：
//! 是错误就按 429/400/502 推一个错误头 —— 网关据此换账号或透传给客户端。

use std::collections::BTreeMap;

use serde_json::{json, Value};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ConversationSink, PluginCtx};

use super::credentials::Credentials;
use super::events::{Decoder, Event, Usage};
use super::{endpoints, errors, models, refresh, request, PROVIDER_ID};
use crate::{http, ids, logging};
use crate::http::AccountRequest;

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

fn usage_chunk(model: &str, id: &str, reason: &str, usage: Usage) -> Vec<u8> {
    let prompt = usage.input + usage.cache_read.max(0);
    let completion = usage.output;
    let mut value = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{ "index": 0, "delta": {}, "finish_reason": reason }],
        "usage": {
            "prompt_tokens": prompt.max(0),
            "completion_tokens": completion.max(0),
            "total_tokens": (prompt + completion).max(0),
            "prompt_tokens_details": { "cached_tokens": usage.cache_read.max(0) },
        },
    });
    if usage.cache_write > 0 {
        value["usage"]["cache_creation_input_tokens"] = json!(usage.cache_write);
    }
    sse(&value)
}

fn error_body(status: u16, message: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "error": {
            "type": if status == 429 { "rate_limit_error" } else { "api_error" },
            "message": message,
        }
    }))
    .unwrap_or_default()
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

/// 发送一次 `generateAssistantResponse`。
async fn ask(
    credentials: &Credentials,
    body: &Value,
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
) -> Result<reqwest::Response, (u16, String)> {
    let client = http::stream_client(proxy, gateway_proxy);
    let region = credentials.region();
    let url = format!("{}/generateAssistantResponse", endpoints::runtime(&region));
    let user_agent = format!(
        "aws-sdk-rust/1.0.0 ua/2.1 os/other lang/rust api/codewhispererstreaming#1.28.3 m/E app/AmazonQ-For-CLI md/appVersion-1.28.3-{}",
        ids::new_message_id()
    );
    let mut request_builder = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", credentials.access))
        .header("x-amzn-codewhisperer-optout", "true")
        .header("x-amzn-kiro-agent-mode", "vibe")
        .header("Content-Type", "application/json")
        .header("Accept", "application/vnd.amazon.eventstream")
        .header("amz-sdk-invocation-id", ids::uuid_v4())
        .header("amz-sdk-request", "attempt=1; max=1")
        .header("x-amz-user-agent", &user_agent)
        .header("User-Agent", &user_agent)
        .json(body);
    if let Some(kind) = credentials.token_type() {
        request_builder = request_builder.header("tokentype", kind);
    }
    let response = request_builder
        .with_account_user_agent()
        .send()
        .await
        .map_err(|error| (502, format!("请求 Kiro 失败：{}", http::describe(&error))))?;
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(response);
    }
    let text = response.text().await.unwrap_or_default();
    Err(errors::failure(status, &text))
}

/// 一次完整转发。
pub async fn run(
    ctx: &PluginCtx,
    conversation: proto::ConversationRequest,
    mut sink: ConversationSink<'_>,
) -> Result<(), String> {
    match forward(ctx, &conversation, &mut sink).await {
        Ok(()) => Ok(()),
        Err((status, message)) => {
            logging::verbose("kiro", &format!("转发失败（{status}）：{message}"));
            head(&mut sink, status).await?;
            sink.send(&error_body(status, &message)).await
        }
    }
}

async fn forward(
    ctx: &PluginCtx,
    conversation: &proto::ConversationRequest,
    sink: &mut ConversationSink<'_>,
) -> Result<(), (u16, String)> {
    let (account_id, mut credentials) = refresh::ensure(ctx, &conversation.account)
        .await
        .map_err(|message| (401, message))?;
    let stored = ctx
        .store_get(PROVIDER_ID, &account_id)
        .await
        .map_err(|error| (502, format!("读取 Kiro 账号失败：{error}")))?;
    refresh::ensure_profile(ctx, &account_id, &stored.secret, &mut credentials)
        .await
        .map_err(|message| (502, message))?;
    let model = if conversation.wire_model.trim().is_empty() {
        conversation
            .body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("auto")
            .to_string()
    } else {
        conversation.wire_model.clone()
    };
    let budget = request::thinking_budget(&conversation.body, &model);
    let native = models::native_of(&account_id, &model);
    let profile = if credentials.is_api_key() {
        String::new()
    } else {
        credentials.profile.clone()
    };
    let body = request::build(
        &conversation.body,
        &model,
        &profile,
        budget,
        native.as_ref(),
    );

    let mut response = ask(
        &credentials,
        &body,
        conversation.proxy.as_ref(),
        &ctx.env.proxy,
    )
    .await;
    if let Err((status, _)) = &response {
        if *status == 401 || *status == 403 {
            // token 被拒：强刷一次再试。刷新失败（API key / 本机登录态也救不回来）
            // 时保留原始错误 —— 那比一条「刷新失败」更贴近真实原因。
            if let Ok((_, refreshed)) = refresh::force_refresh(ctx, &account_id).await {
                credentials = refreshed;
                response = ask(
                    &credentials,
                    &body,
                    conversation.proxy.as_ref(),
                    &ctx.env.proxy,
                )
                .await;
            }
        }
    }
    let response = response?;

    let names = request::tool_names(&conversation.body);
    let split_thinking = budget.is_some();
    let mut decoder = Decoder::new(response, &model, &account_id, split_thinking);
    let id = format!("chatcmpl-{}", ids::new_message_id());
    let first = decoder.next().await.map_err(|error| (502, error))?;
    if let Some(Event::Error { status, text }) = &first {
        let status = status.unwrap_or(502);
        return Err((status, text.clone()));
    }
    head(sink, 200).await.map_err(|message| (502, message))?;
    sink.send(&chunk(
        &model,
        &id,
        json!({ "role": "assistant", "content": "" }),
        None,
    ))
    .await
    .map_err(|message| (502, message))?;

    let mut tool_index: i64 = -1;
    let mut tool_open = false;
    let mut pending = first;
    loop {
        let event = match pending.take() {
            Some(event) => Some(event),
            None => decoder.next().await.map_err(|message| (502, message))?,
        };
        let Some(event) = event else {
            break;
        };
        match event {
            Event::Text(text) => {
                sink.send(&chunk(&model, &id, json!({ "content": text }), None))
                    .await
                    .map_err(|message| (502, message))?;
            }
            Event::Think(text) => {
                sink.send(&chunk(
                    &model,
                    &id,
                    json!({ "reasoning_content": text }),
                    None,
                ))
                .await
                .map_err(|message| (502, message))?;
            }
            Event::ToolStart { id: call_id, name } => {
                tool_index += 1;
                tool_open = true;
                let name = names.get(&name).cloned().unwrap_or(name);
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
            Event::ToolArgs(args) => {
                if !tool_open {
                    continue;
                }
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
            Event::Stop { reason, usage } => {
                sink.send(&usage_chunk(&model, &id, &reason, usage))
                    .await
                    .map_err(|message| (502, message))?;
                sink.send(b"data: [DONE]\n\n")
                    .await
                    .map_err(|message| (502, message))?;
                return Ok(());
            }
            Event::Error { status, text } => {
                let status = status.unwrap_or(502);
                logging::verbose("kiro", &format!("流内错误（{status}）：{text}"));
                let payload = json!({
                    "error": {
                        "type": if status == 429 { "rate_limit_error" } else { "api_error" },
                        "message": text,
                    }
                });
                sink.send(&sse(&payload))
                    .await
                    .map_err(|message| (502, message))?;
                sink.send(b"data: [DONE]\n\n")
                    .await
                    .map_err(|message| (502, message))?;
                return Ok(());
            }
        }
    }
    // 事件流异常结束：给一个收尾帧，别让客户端一直等。
    sink.send(&usage_chunk(&model, &id, "stop", Usage::default()))
        .await
        .map_err(|message| (502, message))?;
    sink.send(b"data: [DONE]\n\n")
        .await
        .map_err(|message| (502, message))
}
