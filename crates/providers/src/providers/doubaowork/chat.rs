use std::collections::BTreeMap;
use std::time::Duration;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ConversationSink, PluginCtx};
use serde_json::{json, Value};

use super::{api, catalog, prompt, stream, upload};
use crate::errors::GatewayError;

fn chunk(id: &str, model: &str, delta: Value, reason: Value) -> Vec<u8> {
    format!("data: {}\n\n",json!({"id":id,"object":"chat.completion.chunk","created":chrono::Utc::now().timestamp(),"model":model,"choices":[{"index":0,"delta":delta,"finish_reason":reason}]})).into_bytes()
}

/// 把 `drain_images` 交出来的图片增量拼成一段 markdown（正文之后补发用）。
fn image_markdown(deltas: Vec<stream::Delta>) -> String {
    deltas
        .into_iter()
        .filter_map(|delta| match delta {
            stream::Delta::Text(text) => Some(text),
            stream::Delta::Reasoning(_) => None,
        })
        .collect()
}

async fn head(sink: &mut ConversationSink<'_>, status: u16, stream: bool) -> Result<(), String> {
    sink.head(proto::StreamHead {
        status,
        headers: BTreeMap::from([(
            "content-type".into(),
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            }
            .into(),
        )]),
        upstream: proto::UpstreamKind::Chat,
    })
    .await
}

pub async fn run(
    ctx: &PluginCtx,
    request: proto::ConversationRequest,
    mut sink: ConversationSink<'_>,
) -> Result<(), String> {
    let mut sent_head = false;
    match forward(ctx, &request, &mut sink, &mut sent_head).await {
        Ok(()) => Ok(()),
        Err(error) if !sent_head => {
            head(&mut sink, error.status_code as u16, false).await?;
            sink.send(api::error_body(&error).to_string().as_bytes())
                .await
        }
        Err(error) => Err(error.message),
    }
}

async fn forward(
    ctx: &PluginCtx,
    request: &proto::ConversationRequest,
    sink: &mut ConversationSink<'_>,
    sent_head: &mut bool,
) -> Result<(), GatewayError> {
    let bridge = prompt::ToolBridge::new(&request.body)?;
    let (text, images) = prompt::content(&request.body, &bridge)?;
    let (account, credentials, control) = api::account(ctx, &request.account, false).await?;
    let (bot, model) =
        catalog::resolve(ctx, &account, &credentials, &control, &request.wire_model).await?;
    let mut attachments = Vec::new();
    for image in images {
        attachments.push(upload::image(&control, &credentials, &image).await?);
    }
    let payload = prompt::payload(&bot, &model, &text, attachments, &request.body)?;
    let client = api::client(request.proxy.as_ref(), &ctx.env.proxy, true)?;
    let response = tokio::time::timeout(
        Duration::from_millis(crate::timeouts::current().headers_ms()),
        api::request(&client, &credentials, "/chat/completion")?
            .json(&payload)
            .send(),
    )
    .await
    .map_err(|_| GatewayError::with_status(504, "等待豆包工作响应头超时"))?
    .map_err(|error| GatewayError::with_status(502, crate::http::describe(&error)))?;
    let status = response.status().as_u16();
    let sse = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    if !(200..300).contains(&status) || !sse {
        let value = response.json::<Value>().await.unwrap_or(Value::Null);
        return Err(api::error(status, &value));
    }
    let mut turn = stream::Turn::new(response);
    let id = crate::ids::random_id("chatcmpl");
    if bridge.active() {
        while turn.next().await?.is_some() {}
        // 出图的链接由 `drain_images` 在流末才交出来（见 `stream::Decoder`：
        // 无水印原图比预览晚到），所以这里先取正文、再补图片链接。
        let text = turn.decoder.text.clone();
        let images = image_markdown(turn.decoder.drain_images());
        if text.is_empty() && images.is_empty() {
            return Err(GatewayError::with_status(502, "豆包工作没有返回回答"));
        }
        // 解析工具调用要用**不含图片链接**的正文：那段 markdown 会让 JSON 解析
        // 失败，进而被当成"工具调用不完整"报错。
        let mut message = bridge.message(&text)?;
        if !images.is_empty() && message["tool_calls"].is_null() {
            let content = message["content"].as_str().unwrap_or("");
            message["content"] = json!(format!("{content}{images}"));
        }
        if !turn.decoder.reasoning.is_empty() {
            message["reasoning_content"] = json!(turn.decoder.reasoning);
        }
        let reason = if message["tool_calls"].is_array() {
            "tool_calls"
        } else {
            "stop"
        };
        if let Some(calls) = message["tool_calls"].as_array_mut() {
            for (index, call) in calls.iter_mut().enumerate() {
                call["index"] = json!(index);
            }
        }
        head(sink, 200, true).await.map_err(GatewayError::from)?;
        *sent_head = true;
        sink.send(&chunk(&id, &request.wire_model, message, Value::Null))
            .await
            .map_err(GatewayError::from)?;
        sink.send(&chunk(&id, &request.wire_model, json!({}), json!(reason)))
            .await
            .map_err(GatewayError::from)?;
    } else {
        let first = tokio::time::timeout(
            Duration::from_millis(crate::timeouts::current().headers_ms()),
            turn.next(),
        )
        .await
        .map_err(|_| GatewayError::with_status(504, "豆包工作未在限时内开始回答"))??;
        head(sink, 200, true).await.map_err(GatewayError::from)?;
        *sent_head = true;
        // 允许第一帧就是流末（`None`）：生图这一类回答可能**只有图片没有正文**，
        // 图片在流末由 `drain_images` 补发，这里把"没有正文"提前判死会误报。
        let mut next = first;
        while let Some(delta) = next {
            let body = match delta {
                stream::Delta::Text(text) => json!({"role":"assistant","content":text}),
                stream::Delta::Reasoning(reasoning) => json!({"reasoning_content":reasoning}),
            };
            sink.send(&chunk(&id, &request.wire_model, body, Value::Null))
                .await
                .map_err(GatewayError::from)?;
            next = turn.next().await?;
        }
        if turn.decoder.text.is_empty() && !turn.decoder.has_images() {
            return Err(GatewayError::with_status(
                502,
                "豆包工作完成响应但没有回答正文",
            ));
        }
        // 图片排在正文之后：这里已经是流末，`drain_images` 拿得到无水印原图。
        let images = image_markdown(turn.decoder.drain_images());
        if !images.is_empty() {
            sink.send(&chunk(
                &id,
                &request.wire_model,
                json!({"role":"assistant","content":images}),
                Value::Null,
            ))
            .await
            .map_err(GatewayError::from)?;
        }
        sink.send(&chunk(&id, &request.wire_model, json!({}), json!("stop")))
            .await
            .map_err(GatewayError::from)?;
    }
    sink.send(b"data: [DONE]\n\n")
        .await
        .map_err(GatewayError::from)
}
