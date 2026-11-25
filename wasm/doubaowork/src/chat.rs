use std::collections::BTreeSet;

use agent2api_wasm_sdk::{self as sdk, json, Result, Value};

use crate::{
    account::{self, Snapshot},
    catalog,
    credentials::Credentials,
    endpoints::{self, ReadEndpoint},
    errors, network, prompt,
    stream::{Decoder, Delta, Parser},
};

pub(super) struct Output {
    id: String,
    model: String,
    started: bool,
}
impl Output {
    fn start(&mut self) -> Result<()> {
        if !self.started {
            sdk::stream_head(200)?;
            self.started = true;
        }
        Ok(())
    }
    fn delta(&mut self, delta: Value, reason: Value) -> Result<()> {
        self.start()?;
        sdk::stream_event(
            &json!({"id":self.id,"object":"chat.completion.chunk","created":sdk::now()/1000,"model":self.model,"choices":[{"index":0,"delta":delta,"finish_reason":reason}]}),
        )
    }
}

pub fn forward(params: &Value) -> Result<Value> {
    let mut output = Output {
        id: format!("chatcmpl-{}", prompt::uuid()?),
        model: params["body"]["model"].as_str().unwrap_or("").into(),
        started: false,
    };
    let result = run(params, &mut output);
    if let Err(error) = &result {
        if output.started {
            let _ = sdk::stream_event(
                &json!({"error":{"message":error.message,"code":error.code,"status":error.status,"upstream_code":error.upstream_code}}),
            );
        }
    }
    result
}
fn run(params: &Value, output: &mut Output) -> Result<Value> {
    let current = account::current()?;
    current.verify()?;
    let prepared = prompt::prepare(&params["body"])?;
    let target = catalog::target(params)?;
    let credentials = current
        .credentials
        .with_runtime_device(target.device_id.as_deref());
    credentials.chat_context()?;
    let mut attachments = Vec::new();
    for image in prepared.images {
        attachments.push(sdk::host(
            "doubao.image.upload",
            json!({"resource":image.id}),
        )?);
        image.remove()?;
    }
    let body = prompt::payload(
        &target.bot,
        &target.model,
        &prepared.text,
        attachments,
        &params["body"],
        false,
    )?;
    let mut decoder = Decoder::default();
    consume(
        &current,
        &credentials,
        ReadEndpoint::Chat,
        &body,
        &mut decoder,
        Some(&mut *output),
        !prepared.bridge.active(),
    )?;
    current.unchanged()?;
    let mut consumed = BTreeSet::new();
    loop {
        let receipt = sdk::host("doubao.turn.receipt", json!({}))?;
        let task = receipt["tasks"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|task| {
                task["completed"] != true
                    && task["task_id"]
                        .as_str()
                        .is_some_and(|id| !consumed.contains(id))
            })
            .cloned();
        let Some(task) = task else {
            break;
        };
        let id = task["task_id"]
            .as_str()
            .ok_or_else(|| errors::protocol("豆包工作任务标识无效"))?
            .to_string();
        if consumed.len() >= 100 || !consumed.insert(id.clone()) {
            return Err(errors::protocol("豆包工作异步任务数量超过限制"));
        }
        consume(
            &current,
            &credentials,
            ReadEndpoint::AsyncChunk,
            &json!({"task_id":id,"seq_start":0,"append_scene":task["append_scene"],"ext":{}}),
            &mut decoder,
            Some(&mut *output),
            !prepared.bridge.active(),
        )?;
    }
    if let Some(error) = decoder.restriction().cloned() {
        sdk::host("doubao.turn.finish", json!({}))?;
        return Err(error);
    }
    if decoder.text.is_empty() && decoder.image_urls().is_empty() {
        return Err(errors::protocol("豆包工作未返回回答或图片"));
    }
    let images = decoder.image_markdown();
    if prepared.bridge.active() {
        let mut message = prepared.bridge.message(&decoder.text)?;
        if message["tool_calls"].is_null() && !images.is_empty() {
            message["content"] = json!(format!(
                "{}{images}",
                message["content"].as_str().unwrap_or("")
            ));
        }
        if !decoder.reasoning.is_empty() {
            message["reasoning_content"] = json!(decoder.reasoning);
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
        output.delta(message, Value::Null)?;
        sdk::host("doubao.turn.finish", json!({}))?;
        output.delta(json!({}), json!(reason))?;
    } else {
        if !images.is_empty() {
            output.delta(json!({"role":"assistant","content":images}), Value::Null)?;
        }
        sdk::host("doubao.turn.finish", json!({}))?;
        output.delta(json!({}), json!("stop"))?;
    }
    sdk::stream_bytes(b"data: [DONE]\n\n")?;
    Ok(Value::Null)
}

pub fn consume(
    current: &Snapshot,
    credentials: &Credentials,
    endpoint: ReadEndpoint,
    body: &Value,
    decoder: &mut Decoder,
    mut output: Option<&mut Output>,
    emit_deltas: bool,
) -> Result<()> {
    credentials.chat_context()?;
    current.unchanged()?;
    let mut response = sdk::http::open(
        "POST",
        &network::url(credentials, endpoint),
        network::headers(credentials, endpoint),
        Some(body),
    )
    .map_err(errors::transport)?;
    if response.status != 200
        || !response
            .header_values("content-type")
            .iter()
            .any(|value| value.to_ascii_lowercase().starts_with("text/event-stream"))
    {
        let status = response.status;
        let bytes = response
            .bytes(endpoints::MAX_RESPONSE)
            .map_err(errors::transport)?;
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        return Err(errors::upstream(status, &value, endpoint));
    }
    decoder.begin_stream();
    let mut parser = Parser::default();
    let mut done = false;
    let mut received = 0usize;
    'read: while let Some(bytes) = response.read().map_err(errors::transport)? {
        received += bytes.len();
        if received > 64 * 1024 * 1024 {
            return Err(errors::protocol("豆包工作流式回答超过大小限制"));
        }
        for (event, data) in parser.push(&bytes)? {
            let (deltas, end) = decoder.frame(&event, &data)?;
            done |= end;
            if emit_deltas && decoder.restriction().is_none() {
                if let Some(output) = output.as_deref_mut() {
                    for delta in deltas {
                        output.delta(
                            match delta {
                                Delta::Text(text) => json!({"role":"assistant","content":text}),
                                Delta::Reasoning(text) => json!({"reasoning_content":text}),
                            },
                            Value::Null,
                        )?;
                    }
                }
            }
        }
        if done {
            break 'read;
        }
    }
    parser.finish()?;
    if !done {
        return Err(errors::protocol(
            "豆包工作在本轮结束前断开，未把部分回答当成成功",
        ));
    }
    current.unchanged()
}
