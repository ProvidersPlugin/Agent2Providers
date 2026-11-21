//! 标准 Chat SSE；首个有效模型增量之前保留错误切换机会，输出后不重放整次请求。
//! 请求兼容规则只来自经过校验的公开目录，模型鉴权只用 llmToken。

use super::{account, client, endpoints, errors, models, reasoning, refresh};
use agent2api_wasm_sdk::{self as sdk, http, json, sse::SseDecoder, Error, Result, Value};

fn body(mut params: Value) -> Result<(Value, &'static str)> {
    let reasoning_level = params["reasoning"].as_str().unwrap_or("").to_string();
    let mut body = sdk::take_body(&mut params).unwrap_or(Value::Null);
    drop(params);
    if !body.is_object() {
        return Err(Error::http(400, "QuWork 请求体必须是 JSON 对象"));
    }
    let requested = body["model"]
        .as_str()
        .filter(|model| !model.is_empty())
        .ok_or_else(|| Error::http(400, "QuWork 请求缺少模型"))?;
    let row = models::find(requested)?;
    body["model"] = row["id"].clone();
    body["stream"] = json!(true);
    if !body["stream_options"].is_object() {
        body["stream_options"] = json!({});
    }
    body["stream_options"]["include_usage"] = json!(true);
    let compat = &row["compat"];
    if let Some(object) = body.as_object_mut() {
        if compat["supportsStore"] == false {
            object.remove("store");
        }
        if compat["maxTokensField"] == "max_tokens" {
            if let Some(limit) = object.remove("max_completion_tokens") {
                object.entry("max_tokens").or_insert(limit);
            }
        }
    }
    reasoning::apply(&mut body, &row, &reasoning_level);
    if let Some(messages) = body["messages"].as_array_mut() {
        for message in messages {
            if compat["supportsDeveloperRole"] == false && message["role"] == "developer" {
                message["role"] = json!("system");
            }
            if compat["requiresReasoningContentOnAssistantMessages"] == true
                && message["role"] == "assistant"
                && message["reasoning_content"].is_null()
            {
                message["reasoning_content"] = message["reasoning"]
                    .as_str()
                    .map(|value| json!(value))
                    .unwrap_or(json!(""));
            }
        }
    }
    let origin = row["quworkModelOrigin"]
        .as_str()
        .and_then(endpoints::model_origin)
        .ok_or_else(|| {
            Error::http(400, "QuWork 模型来源尚未确认，请先刷新模型列表")
                .with_code("local_precondition")
        })?;
    Ok((body, origin))
}

#[derive(Default)]
struct Output {
    pending: Vec<Vec<u8>>,
    bytes: usize,
    started: bool,
    meaningful: bool,
    finished: bool,
    done: bool,
}

impl Output {
    fn start(&mut self) -> Result<()> {
        if !self.started {
            sdk::stream_head(200)?;
            self.started = true;
        }
        for bytes in self.pending.drain(..) {
            sdk::stream_bytes(&bytes)?;
        }
        self.bytes = 0;
        Ok(())
    }
    fn event(&mut self, data: &str) -> Result<()> {
        if self.done {
            return Ok(());
        }
        if data.trim() == "[DONE]" {
            if !self.meaningful && !self.finished {
                return Err(Error::http(502, "QuWork 没有返回有效回答"));
            }
            self.start()?;
            sdk::stream_bytes(b"data: [DONE]\n\n")?;
            self.done = true;
            return Ok(());
        }
        let value: Value = serde_json::from_str(data)
            .map_err(|_| Error::http(502, "QuWork 流式事件不是有效 JSON"))?;
        if let Some(error) = errors::event(&value) {
            return Err(error);
        }
        let choices = match value["choices"].as_array() {
            Some(choices) => choices,
            None if value["usage"].is_object() => {
                let mut chunk = value;
                chunk["choices"] = json!([]);
                return self.event(&chunk.to_string());
            }
            None => return Err(Error::http(502, "QuWork 流式事件缺少 choices")),
        };
        for choice in choices {
            let delta = &choice["delta"];
            if !delta.is_null() && !delta.is_object() {
                return Err(Error::http(502, "QuWork 流式增量格式无效"));
            }
            self.meaningful |= delta["content"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
                || delta["tool_calls"]
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
                || delta["function_call"].is_object()
                || delta["reasoning_content"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty())
                || delta["reasoning"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty())
                || delta["refusal"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty());
            if delta["refusal"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
            {
                return Err(errors::upstream(
                    403,
                    &json!({"error":{"code":"content_filter"}}),
                    "model",
                ));
            }
            if let Some(reason) = choice["finish_reason"]
                .as_str()
                .filter(|reason| !reason.is_empty())
            {
                if reason == "content_filter" {
                    return Err(errors::upstream(
                        403,
                        &json!({ "error": { "code": "content_filter" } }),
                        "model",
                    ));
                }
                self.finished = true;
            }
        }
        let bytes = format!("data: {data}\n\n").into_bytes();
        if self.started {
            return sdk::stream_bytes(&bytes);
        }
        self.bytes = self.bytes.saturating_add(bytes.len());
        if self.bytes > 1024 * 1024 {
            return Err(Error::http(502, "QuWork 连续返回空事件，未收到有效回答"));
        }
        self.pending.push(bytes);
        if self.meaningful || self.finished {
            self.start()?;
        }
        Ok(())
    }
    fn consume(&mut self, mut response: http::Response, record: &Value) -> Result<()> {
        let content_type = response.headers["content-type"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase();
        if content_type.contains("application/json") {
            let reply = client::read(response)?;
            if let Some(error) = errors::event(&reply.body) {
                return Err(error);
            }
            let choices = reply.body["choices"]
                .as_array()
                .filter(|choices| !choices.is_empty())
                .ok_or_else(|| Error::http(502, "QuWork 返回了非对话的 JSON"))?;
            let mut chunk = reply.body.clone();
            chunk["object"] = json!("chat.completion.chunk");
            let mut converted = Vec::new();
            for choice in choices {
                if !choice.is_object() {
                    return Err(Error::http(502, "QuWork 回答结构无效"));
                }
                let mut choice = choice.clone();
                choice["delta"] = choice["message"].clone();
                if let Some(object) = choice.as_object_mut() {
                    object.remove("message");
                }
                converted.push(choice);
            }
            chunk["choices"] = json!(converted);
            account::unchanged(record)?;
            self.event(&chunk.to_string())?;
            return self.event("[DONE]");
        }
        if content_type.contains("text/html") {
            return Err(Error::http(502, "QuWork 返回了网页而不是模型回答"));
        }
        let mut decoder = SseDecoder::default();
        while let Some(bytes) = response.read().map_err(client::transport)? {
            account::unchanged(record)?;
            for event in decoder.push(&bytes)? {
                self.event(&event)?;
            }
            if self.done {
                return Ok(());
            }
        }
        Err(Error::http(
            502,
            "QuWork 上游流提前中断，未收到完整结束标记",
        ))
    }
}

pub fn forward(params: Value) -> Result<()> {
    let (body, origin) = body(params)?;
    let token = refresh::model_token()?;
    let record = sdk::account()?;
    if account::text(&refresh::snapshot()?, &["llmToken"]) != token {
        return Err(
            Error::http(409, "QuWork 模型凭据在请求准备期间已变化，请重新发起")
                .with_code("identity_changed"),
        );
    }
    account::unchanged(&record)?;
    let mut headers = client::headers(Some(&token), false);
    headers["accept"] = json!("text/event-stream, application/json");
    let response = http::open(
        "POST",
        &format!("{origin}/chat/completions"),
        headers,
        Some(&body),
    )
    .map_err(client::transport)?;
    account::unchanged(&record)?;
    if !(200..300).contains(&response.status) {
        let reply = client::read(response)?;
        return Err(errors::upstream(reply.status, &reply.body, "model"));
    }
    let mut output = Output::default();
    match output.consume(response, &record) {
        Ok(()) => Ok(()),
        Err(error) if output.started => {
            sdk::stream_event(&errors::stream_error(&error))?;
            Err(error)
        }
        Err(error) => Err(error),
    }
}
