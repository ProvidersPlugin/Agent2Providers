//! Cline上游stream=true是原生Chat chunk。首个有效增量前拦业务失败，输出后不重放。

use super::{client, endpoints, errors, headers, payload, refresh};
use agent2api_wasm_sdk::{self as sdk, http, json, sse::SseDecoder, Error, Result, Value};

#[derive(Default)]
struct Output {
    pending: Vec<Vec<u8>>,
    size: usize,
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
        self.size = 0;
        Ok(())
    }
    fn event(&mut self, data: &str) -> Result<()> {
        if self.done {
            return Ok(());
        }
        if data.trim() == "[DONE]" {
            if !self.meaningful && !self.finished {
                return Err(Error::http(502, "Cline 未返回有效回答或结束原因"));
            }
            self.start()?;
            sdk::stream_bytes(b"data: [DONE]\n\n")?;
            self.done = true;
            return Ok(());
        }
        let value: Value = serde_json::from_str(data)
            .map_err(|_| Error::http(502, "Cline 流式事件不是有效 JSON"))?;
        if let Some(error) = errors::event(&value) {
            return Err(error);
        }
        let choices = value["choices"]
            .as_array()
            .ok_or_else(|| Error::http(502, "Cline 流式事件缺少choices"))?;
        if choices.is_empty() && !value["usage"].is_object() {
            return Err(Error::http(502, "Cline 流式事件既无回答也无有效计量"));
        }
        for choice in choices {
            if !choice.is_object() {
                return Err(Error::http(502, "Cline 流式回答格式异常"));
            }
            let delta = &choice["delta"];
            if !delta.is_null() && !delta.is_object() {
                return Err(Error::http(502, "Cline 流式增量格式异常"));
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
                || delta["reasoning_details"]
                    .as_array()
                    .is_some_and(|items| !items.is_empty());
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
        let event = value.to_string();
        let bytes = format!("data: {event}\n\n").into_bytes();
        if self.started {
            return sdk::stream_bytes(&bytes);
        }
        self.size = self.size.saturating_add(bytes.len());
        if self.size > 1024 * 1024 {
            return Err(Error::http(502, "Cline 连续返回空事件，没有有效回答"));
        }
        self.pending.push(bytes);
        if self.meaningful || self.finished {
            self.start()?;
        }
        Ok(())
    }
    fn consume(&mut self, mut response: http::Response, secret: &Value) -> Result<()> {
        let content_type = response.headers["content-type"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase();
        if content_type.contains("application/json") {
            let reply = client::read(response)?;
            let mut value = reply.data()?;
            // Cline JSON may put the completion in data but keep usage on the outer envelope.
            if value.is_object() && value.get("usage").is_none() {
                if let Some(usage) = reply.body.get("usage") {
                    value["usage"] = usage.clone();
                }
            }
            if let Some(error) = errors::event(&value) {
                return Err(error);
            }
            let choices = value["choices"]
                .as_array()
                .filter(|items| !items.is_empty())
                .ok_or_else(|| Error::http(502, "Cline 返回了非对话的JSON"))?;
            let mut chunk = value.clone();
            chunk["object"] = json!("chat.completion.chunk");
            let mut rows = Vec::new();
            for choice in choices {
                if !choice.is_object() {
                    return Err(Error::http(502, "Cline 回答结构异常"));
                }
                let mut row = choice.clone();
                row["delta"] = row["message"].clone();
                if let Some(object) = row.as_object_mut() {
                    object.remove("message");
                }
                rows.push(row);
            }
            chunk["choices"] = json!(rows);
            refresh::confirm(secret)?;
            self.event(&chunk.to_string())?;
            return self.event("[DONE]");
        }
        if content_type.contains("text/html") {
            return Err(Error::http(502, "Cline 返回了网页而不是模型回答"));
        }
        let mut decoder = SseDecoder::default();
        while let Some(bytes) = response.read().map_err(client::transport)? {
            let events = decoder.push(&bytes)?;
            if !self.started || events.iter().any(|data| data.trim() == "[DONE]") {
                refresh::confirm(secret)?;
            }
            for data in events {
                self.event(&data)?;
            }
            if self.done {
                return Ok(());
            }
        }
        Err(Error::http(502, "Cline 上游流提前中断，未收到完整结束标记"))
    }
}

pub fn forward(params: &Value) -> Result<()> {
    let session = format!("sess_{}", sdk::now());
    let body = payload::build(&params["body"], &session)?;
    let secret = refresh::ensure(false)?;
    let token = super::account::access(&secret)?;
    let headers = headers::chat(&token, &session)?;
    // 读取客户端配置期间用户可能更新同一账号凭证，发送前复核一次，不能拼旧token与新来源。
    refresh::confirm(&secret)?;
    let response = http::open(
        "POST",
        &format!("{}/chat/completions", endpoints::API),
        headers,
        Some(&body),
    )
    .map_err(client::transport)?;
    if !(200..300).contains(&response.status) {
        let reply = client::read(response)?;
        return Err(errors::upstream(reply.status, &reply.body));
    }
    let mut output = Output::default();
    match output.consume(response, &secret) {
        Ok(()) => Ok(()),
        Err(error) if output.started => {
            sdk::stream_event(&errors::stream_error(&error))?;
            Err(error)
        }
        Err(error) => Err(error),
    }
}
