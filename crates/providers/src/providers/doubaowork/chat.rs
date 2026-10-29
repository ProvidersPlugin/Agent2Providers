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
        if turn.decoder.text.is_empty() {
            return Err(GatewayError::with_status(502, "豆包工作没有返回回答"));
        }
        let mut message = bridge.message(&turn.decoder.text)?;
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
        .map_err(|_| GatewayError::with_status(504, "豆包工作未在限时内开始回答"))??
        .ok_or_else(|| GatewayError::with_status(502, "豆包工作没有返回回答"))?;
        head(sink, 200, true).await.map_err(GatewayError::from)?;
        *sent_head = true;
        let mut next = Some(first);
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
        if turn.decoder.text.is_empty() {
            return Err(GatewayError::with_status(
                502,
                "豆包工作完成响应但没有回答正文",
            ));
        }
        sink.send(&chunk(&id, &request.wire_model, json!({}), json!("stop")))
            .await
            .map_err(GatewayError::from)?;
    }
    sink.send(b"data: [DONE]\n\n")
        .await
        .map_err(GatewayError::from)
}
