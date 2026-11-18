//! 上游仅收 SSE；保留原消息及工具历史，按当前模型目录映射官方思考方言。
//! 空帧不提交成功响应，先排除流内业务失败；已输出后绝不从头重试。

use super::{account, errors, models, reasoning, upstream, version};
use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};

#[derive(Default)]
struct Output {
    pending: Vec<Vec<u8>>,
    pending_bytes: usize,
    started: bool,
    completed: bool,
    meaningful: bool,
    finished: bool,
    account: Value,
}

impl Output {
    fn new(account: Value) -> Self {
        Self {
            account,
            ..Self::default()
        }
    }

    fn start(&mut self) -> Result<()> {
        account::validate_current(&self.account)?;
        if !self.started {
            sdk::stream_head(200)?;
            self.started = true;
        }
        for bytes in self.pending.drain(..) {
            sdk::stream_bytes(&bytes)?;
        }
        self.pending_bytes = 0;
        Ok(())
    }

    fn event(&mut self, data: &str) -> Result<()> {
        account::validate_current(&self.account)?;
        if self.completed {
            return Ok(());
        }
        if data.trim() == "[DONE]" {
            if !self.meaningful && !self.finished {
                return Err(Error::http(502, "LobsterAI 未返回有效回答或结束原因"));
            }
            self.start()?;
            sdk::stream_bytes(b"data: [DONE]\n\n")?;
            self.completed = true;
            return Ok(());
        }
        let value: Value = serde_json::from_str(data)
            .map_err(|_| Error::http(502, "LobsterAI 流式事件不是有效 JSON"))?;
        if let Some(error) = errors::event(&value) {
            return Err(error);
        }
        let choices = value["choices"]
            .as_array()
            .ok_or_else(|| Error::http(502, "LobsterAI 流式事件缺少 choices"))?;
        for choice in choices {
            let delta = &choice["delta"];
            if !delta.is_null() && !delta.is_object() {
                return Err(Error::http(502, "LobsterAI 流式增量格式无效"));
            }
            self.meaningful |= delta["content"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
                || delta["content"]
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
                || delta["tool_calls"]
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
                || delta["function_call"].is_object()
                || delta["reasoning_content"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty())
                || delta["reasoning"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty());
            if let Some(reason) = choice["finish_reason"]
                .as_str()
                .filter(|reason| !reason.is_empty())
            {
                if matches!(reason, "content_filter" | "safety") {
                    return Err(errors::upstream(403, &json!({ "code": "content_filter" })));
                }
                self.finished = true;
            }
        }
        let mut chunk = value;
        if let Some(usage) = chunk.get_mut("usage").and_then(Value::as_object_mut) {
            if !usage.contains_key("completion_tokens_details") {
                if let Some(reasoning) = usage
                    .get("reasoning_tokens")
                    .cloned()
                    .filter(|value| value.is_u64())
                {
                    usage.insert(
                        "completion_tokens_details".into(),
                        json!({ "reasoning_tokens": reasoning }),
                    );
                }
            }
        }
        let bytes = format!("data: {chunk}\n\n").into_bytes();
        if self.started {
            return sdk::stream_bytes(&bytes);
        }
        self.pending_bytes = self.pending_bytes.saturating_add(bytes.len());
        if self.pending_bytes > 1024 * 1024 {
            return Err(Error::http(502, "LobsterAI 连续返回空事件，未收到有效回答"));
        }
        self.pending.push(bytes);
        if self.meaningful || self.finished {
            self.start()?;
        }
        Ok(())
    }

    fn consume(&mut self, mut response: http::Response) -> Result<()> {
        let content_type = response
            .headers
            .as_object()
            .and_then(|headers| {
                headers
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case("content-type"))
            })
            .and_then(|(_, value)| value.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if content_type.contains("application/json") {
            let value = upstream::read_json(response)?;
            if let Some(error) = errors::event(&value) {
                return Err(error);
            }
            let choices = value["choices"]
                .as_array()
                .ok_or_else(|| Error::http(502, "LobsterAI 返回了非对话的 JSON"))?;
            if choices.is_empty() {
                return Err(Error::http(502, "LobsterAI 返回了空回答"));
            }
            let mut chunk = value.clone();
            chunk["object"] = json!("chat.completion.chunk");
            chunk["choices"] = Value::Array(
                choices
                    .iter()
                    .map(|choice| {
                        let mut result = choice.clone();
                        result["delta"] = choice["message"].clone();
                        if let Some(object) = result.as_object_mut() {
                            object.remove("message");
                        }
                        result
                    })
                    .collect(),
            );
            self.event(&chunk.to_string())?;
            return self.event("[DONE]");
        }
        if content_type.contains("text/html") {
            return Err(Error::http(502, "LobsterAI 返回了网页而不是模型回答"));
        }
        let mut decoder = Decoder::default();
        while let Some(bytes) = response.read().map_err(upstream::transport)? {
            for (event, data) in decoder.push(&bytes)? {
                if event == "error" {
                    return Err(errors::stream_error(&data));
                }
                self.event(&data)?;
                if self.completed {
                    return Ok(());
                }
            }
        }
        for (event, data) in decoder.finish()? {
            if event == "error" {
                return Err(errors::stream_error(&data));
            }
            self.event(&data)?;
        }
        if self.completed {
            return Ok(());
        }
        Err(Error::http(
            502,
            "LobsterAI 上游流提前中断，未收到完整结束标记",
        ))
    }
}

// SDK 解码器仅保留 data；本家 event:error 必须保留事件名，否则纯文字错误失去分类。
#[derive(Default)]
struct Decoder {
    line: Vec<u8>,
    data: Vec<String>,
    event: String,
    bytes: usize,
    skip_lf: bool,
}

impl Decoder {
    fn push(&mut self, input: &[u8]) -> Result<Vec<(String, String)>> {
        let mut events = Vec::new();
        for byte in input {
            if self.skip_lf {
                self.skip_lf = false;
                if *byte == b'\n' {
                    continue;
                }
            }
            if matches!(*byte, b'\r' | b'\n') {
                self.line(&mut events)?;
                self.skip_lf = *byte == b'\r';
            } else {
                if self.line.len().saturating_add(self.bytes) >= 1024 * 1024 {
                    return Err(Error::http(502, "LobsterAI 单个流式事件超过 1 MiB"));
                }
                self.line.push(*byte);
            }
        }
        Ok(events)
    }

    fn line(&mut self, events: &mut Vec<(String, String)>) -> Result<()> {
        if self.line.is_empty() {
            if !self.data.is_empty() || self.event == "error" {
                events.push((std::mem::take(&mut self.event), self.data.join("\n")));
            }
            self.event.clear();
            self.data.clear();
            self.bytes = 0;
        } else {
            let line = std::str::from_utf8(&self.line)
                .map_err(|_| Error::http(502, "LobsterAI 流式事件不是有效 UTF-8"))?;
            if let Some(data) = line.strip_prefix("data:") {
                let data = data.strip_prefix(' ').unwrap_or(data);
                self.bytes = self.bytes.saturating_add(data.len() + 1);
                self.data.push(data.to_string());
            } else if let Some(event) = line.strip_prefix("event:") {
                let event = event.trim();
                if event.len() > 128 {
                    return Err(Error::http(502, "LobsterAI 事件名称超过限制"));
                }
                self.event = event.to_string();
            }
        }
        self.line.clear();
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<(String, String)>> {
        let mut events = Vec::new();
        if !self.line.is_empty() {
            self.line(&mut events)?;
        }
        self.line(&mut events)?;
        Ok(events)
    }
}

fn body(params: &Value) -> Result<Value> {
    let mut body = params["body"].clone();
    let object = body
        .as_object_mut()
        .ok_or_else(|| Error::http(400, "请求体必须是 JSON 对象"))?;
    let requested = object
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| Error::http(400, "请求缺少模型名称"))?;
    let model = models::wire_id(requested)?;
    let description = models::for_request(&model)?;
    if object
        .get("messages")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
    {
        return Err(Error::http(400, "LobsterAI 对话至少需要一条消息"));
    }
    if let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            if message["role"] == "developer" {
                message["role"] = json!("system");
            }
        }
    }
    if object.get("tool_choice").is_some_and(|value| {
        value.is_null()
            || value.as_str().is_some_and(|text| {
                text.trim().is_empty() || text.trim().eq_ignore_ascii_case("none")
            })
    }) {
        object.remove("tool_choice");
    }
    object.remove("prompt_cache_key");
    object.insert("model".into(), json!(model));
    object.insert("stream".into(), json!(true));
    if object.get("stream_options").is_none_or(Value::is_null) {
        object.insert("stream_options".into(), json!({ "include_usage": true }));
    } else if let Some(options) = object
        .get_mut("stream_options")
        .and_then(Value::as_object_mut)
    {
        options.insert("include_usage".into(), json!(true));
    }
    reasoning::apply(&mut body, params, &description)?;
    Ok(body)
}

pub fn forward(params: &Value) -> Result<()> {
    let account = account::prepared()?;
    let payload = body(params)?;
    account::validate_current(&account)?;
    let token = account::access(&account["secret"])?;
    let mut headers = upstream::headers(Some(&token), Some(&version::resolve()));
    account::validate_current(&account)?;
    headers["accept"] = json!("text/event-stream, application/json");
    let response = http::open(
        "POST",
        &format!("{}/api/proxy/v1/chat/completions", upstream::API),
        headers,
        Some(&payload),
    )
    .map_err(upstream::transport)?;
    if !(200..300).contains(&response.status) {
        upstream::read_json(response)?;
        return Err(Error::http(502, "LobsterAI 对话请求未成功"));
    }
    let mut output = Output::new(account);
    match output.consume(response) {
        Ok(()) => Ok(()),
        Err(error) if output.started => {
            sdk::stream_event(&errors::stream_event(&error))?;
            Err(error)
        }
        Err(error) => Err(error),
    }
}
