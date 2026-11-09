use agent2api_wasm_sdk::{self as sdk, http, json, sse::SseDecoder, Error, Result, Value};

use super::{account, errors, models, upstream};

#[derive(Default)]
struct Output {
    pending: Vec<Vec<u8>>,
    pending_size: usize,
    text: String,
    started: bool,
    complete: bool,
    normal_finish: bool,
}

fn candidate(text: &str) -> bool {
    let text = text.trim_start();
    ["参数错误", "三方请求失败", "模型「"].iter().any(|prefix| prefix.starts_with(text) || text.starts_with(prefix))
}

impl Output {
    fn start(&mut self) -> Result<()> {
        if !self.started { sdk::stream_head(200)?; self.started = true; }
        for bytes in self.pending.drain(..) { sdk::stream_bytes(&bytes)?; }
        self.pending_size = 0;
        Ok(())
    }

    fn event(&mut self, data: &str) -> Result<()> {
        if self.complete { return Ok(()); }
        if data.trim() == "[DONE]" {
            if !self.started {
                if !self.normal_finish {
                    if let Some(error) = errors::silent(&self.text) { return Err(error); }
                }
                if self.text.is_empty() && !self.normal_finish {
                    return Err(Error::http(502, "AtomCode 未返回有效内容或结束原因"));
                }
            }
            self.start()?;
            sdk::stream_bytes(b"data: [DONE]\n\n")?;
            self.complete = true;
            return Ok(());
        }
        let value: Value = serde_json::from_str(data).map_err(|_| Error::new("AtomCode 流式事件格式无效"))?;
        if value.get("error").is_some_and(|value| !value.is_null()) {
            let status = value["error"]["status"].as_u64()
                .or_else(|| value["error"]["code"].as_u64())
                .or_else(|| value["error"]["code"].as_str().and_then(|value| value.parse().ok()))
                .filter(|status| (400..600).contains(status)).unwrap_or(502);
            return Err(errors::http_error(status as u16, &value));
        }
        let choices = value["choices"].as_array().ok_or_else(|| Error::new("AtomCode 流式事件缺少 choices"))?;
        let mut decisive = false;
        for choice in choices {
            let delta = &choice["delta"];
            if let Some(content) = delta["content"].as_str() {
                if !self.started && self.text.len().saturating_add(content.len()) <= 8192 { self.text.push_str(content); }
                else if !content.is_empty() { decisive = true; }
            }
            decisive |= delta["tool_calls"].as_array().is_some_and(|calls| !calls.is_empty())
                || delta["reasoning_content"].as_str().is_some_and(|text| !text.is_empty())
                || delta["reasoning"].as_str().is_some_and(|text| !text.is_empty());
            if let Some(reason) = choice["finish_reason"].as_str() {
                if matches!(reason, "content_filter" | "safety") {
                    return Err(errors::http_error(403, &json!({ "code": "content_filter" })));
                }
                self.normal_finish = true;
            }
        }
        let bytes = format!("data: {data}\n\n").into_bytes();
        if self.started { sdk::stream_bytes(&bytes)?; return Ok(()); }
        self.pending_size += bytes.len();
        self.pending.push(bytes);
        if decisive || !self.text.is_empty() && !candidate(&self.text) || self.normal_finish || self.pending_size > 64 * 1024 {
            self.start()?;
        }
        Ok(())
    }

    fn consume(&mut self, mut response: http::Response) -> Result<()> {
        let content_type = response.headers["content-type"].as_str().unwrap_or("").to_ascii_lowercase();
        if content_type.contains("application/json") {
            let value = response.json()?;
            if value.get("error").is_some_and(|value| !value.is_null()) { return Err(errors::http_error(502, &value)); }
            let choices = value["choices"].as_array().ok_or_else(|| Error::new("AtomCode 响应缺少 choices"))?;
            let text = choices.first().and_then(|choice| choice["message"]["content"].as_str()).unwrap_or("");
            if choices.iter().all(|choice| choice["finish_reason"].is_null()) {
                if let Some(error) = errors::silent(text) { return Err(error); }
            }
            let mut chunk = value.clone();
            chunk["object"] = json!("chat.completion.chunk");
            chunk["choices"] = Value::Array(choices.iter().map(|choice| {
                let mut choice = choice.clone();
                choice["delta"] = choice["message"].clone();
                if let Some(choice) = choice.as_object_mut() { choice.remove("message"); }
                choice
            }).collect());
            self.event(&chunk.to_string())?;
            return self.event("[DONE]");
        }
        if content_type.contains("text/html") { return Err(Error::http(502, "AtomCode 返回了网页而不是模型回答")); }
        let mut decoder = SseDecoder::default();
        while let Some(bytes) = response.read()? {
            for event in decoder.push(&bytes)? { self.event(&event)?; }
            if self.complete { return Ok(()); }
        }
        Err(Error::http(502, "AtomCode 上游流提前中断，未收到完整结束标记"))
    }
}

pub fn forward(params: &Value) -> Result<()> {
    let token = account::ensure()?;
    let mut body = params["body"].clone();
    if !body.is_object() { return Err(Error::http(400, "请求体必须是 JSON 对象")); }
    let requested = body["model"].as_str().filter(|name| !name.trim().is_empty())
        .ok_or_else(|| Error::http(400, "请求缺少模型名称"))?;
    body["model"] = json!(models::wire_id(requested, params["account"].as_str().unwrap_or(""))?);
    body["stream"] = json!(true);
    if body.get("stream_options").is_none() { body["stream_options"] = json!({ "include_usage": true }); }
    let response = http::open("POST", &format!("{}/chat/completions", upstream::base("gatewayBase")), upstream::headers(Some(&token)), Some(&body))?;
    if !(200..300).contains(&response.status) {
        let status = response.status;
        let bytes = response.bytes(1024 * 1024)?;
        let error: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        return Err(errors::http_error(status, &error));
    }
    let mut output = Output::default();
    match output.consume(response) {
        Ok(()) => Ok(()),
        Err(error) if output.started => {
            // 已下发内容后只报告中断，不能把整个请求悄悄从头重发。
            sdk::stream_event(&errors::error_event(&error))?;
            Err(error)
        }
        Err(error) => Err(error),
    }
}
