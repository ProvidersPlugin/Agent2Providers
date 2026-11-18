use agent2api_wasm_sdk::{self as sdk, http, json, sse::SseDecoder, Error, Result, Value};

use crate::errors;

#[derive(Default)]
pub struct Output {
    pending: Vec<String>,
    pending_bytes: usize,
    pub started: bool,
    meaningful: bool,
    done: bool,
}

impl Output {
    fn start(&mut self) -> Result<()> {
        if !self.started {
            sdk::stream_head(200)?;
            self.started = true;
        }
        for data in self.pending.drain(..) {
            sdk::stream_bytes(format!("data: {data}\n\n").as_bytes())?;
        }
        self.pending_bytes = 0;
        Ok(())
    }

    fn event(&mut self, raw: &str, protected: &[String]) -> Result<()> {
        if self.done {
            return Ok(());
        }
        if raw.trim() == "[DONE]" {
            if !self.meaningful {
                return Err(Error::http(502, "百度搭子未返回内容或结束原因"));
            }
            self.start()?;
            sdk::stream_bytes(b"data: [DONE]\n\n")?;
            self.done = true;
            return Ok(());
        }
        let value: Value = serde_json::from_str(raw)
            .map_err(|_| Error::http(502, "百度搭子流式事件不是有效 JSON"))?;
        if errors::failed(&value) {
            return Err(errors::upstream(200, &value, protected));
        }
        let choices = value["choices"].as_array();
        if choices.is_none() && !value["usage"].is_object() {
            return Err(Error::http(502, "百度搭子流式事件缺少 choices 或 usage"));
        }
        for choice in choices.into_iter().flatten() {
            if !choice.is_object() {
                return Err(Error::http(502, "百度搭子流式回答格式异常"));
            }
            if choice["finish_reason"] == "content_filter" {
                return Err(errors::upstream(
                    403,
                    &json!({ "error": { "type": "content_filter", "message": "本次内容被上游内容审核拒绝" } }),
                    protected,
                ));
            }
            let delta = &choice["delta"];
            self.meaningful |= ["content", "reasoning_content", "reasoning"]
                .iter()
                .any(|key| delta[*key].as_str().is_some_and(|text| !text.is_empty()))
                || delta["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| !calls.is_empty())
                || delta["function_call"].is_object()
                || choice["finish_reason"].as_str().is_some();
        }
        if self.started {
            sdk::stream_bytes(format!("data: {raw}\n\n").as_bytes())?;
        } else {
            self.pending_bytes = self.pending_bytes.saturating_add(raw.len());
            if self.pending_bytes > 64 * 1024 && !self.meaningful {
                return Err(Error::http(
                    502,
                    "百度搭子前置事件超过限制，尚未得到有效回答",
                ));
            }
            self.pending.push(raw.to_string());
            if self.meaningful {
                self.start()?;
            }
        }
        Ok(())
    }

    fn json_response(&mut self, bytes: &[u8], protected: &[String]) -> Result<()> {
        let mut value: Value = serde_json::from_slice(bytes)
            .map_err(|_| Error::http(502, "百度搭子模型响应不是有效 JSON"))?;
        if errors::failed(&value) {
            return Err(errors::upstream(200, &value, protected));
        }
        let choices = value["choices"]
            .as_array()
            .filter(|choices| !choices.is_empty())
            .ok_or_else(|| Error::http(502, "百度搭子模型响应缺少 choices"))?;
        let mut deltas = Vec::with_capacity(choices.len());
        for (index, item) in choices.iter().enumerate() {
            if !item["message"].is_object() {
                return Err(Error::http(502, "百度搭子模型响应缺少 message"));
            }
            let mut choice = item.clone();
            let mut delta = choice["message"].clone();
            if let Some(calls) = delta["tool_calls"].as_array_mut() {
                for (index, call) in calls.iter_mut().enumerate() {
                    if !call.is_object() {
                        return Err(Error::http(502, "百度搭子工具调用格式异常"));
                    }
                    call["index"] = json!(index);
                }
            }
            choice["delta"] = delta;
            if choice["index"].is_null() {
                choice["index"] = json!(index);
            }
            if let Some(choice) = choice.as_object_mut() {
                choice.remove("message");
            }
            deltas.push(choice);
        }
        value["object"] = json!("chat.completion.chunk");
        value["choices"] = json!(deltas);
        self.event(&value.to_string(), protected)?;
        self.event("[DONE]", protected)
    }

    pub fn consume(
        &mut self,
        mut response: http::Response,
        protected: &[String],
        current: &crate::account::Snapshot,
    ) -> Result<()> {
        current.unchanged()?;
        let content_type = response.headers["content-type"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase();
        if content_type.contains("application/json") {
            let bytes = response.bytes(4 * 1024 * 1024)?;
            current.unchanged()?;
            return self.json_response(&bytes, protected);
        }
        if content_type.contains("text/html") {
            return Err(Error::http(502, "百度搭子返回了网页而不是对话内容"));
        }
        let mut decoder = SseDecoder::default();
        let mut decided = false;
        let mut prefix = Vec::new();
        while let Some(bytes) = response.read()? {
            current.unchanged()?;
            let bytes = if !decided {
                if prefix.len().saturating_add(bytes.len()) > 1024 * 1024 {
                    return Err(Error::http(502, "百度搭子响应前缀超过大小限制"));
                }
                prefix.extend_from_slice(&bytes);
                let first = prefix.iter().find(|byte| !byte.is_ascii_whitespace());
                let Some(first) = first else {
                    continue;
                };
                if *first == b'{' {
                    let mut body = prefix;
                    while let Some(bytes) = response.read()? {
                        current.unchanged()?;
                        if body.len().saturating_add(bytes.len()) > 4 * 1024 * 1024 {
                            return Err(Error::http(502, "百度搭子错误响应超过大小限制"));
                        }
                        body.extend_from_slice(&bytes);
                    }
                    current.unchanged()?;
                    return self.json_response(&body, protected);
                }
                decided = true;
                std::mem::take(&mut prefix)
            } else {
                bytes
            };
            for data in decoder.push(&bytes)? {
                self.event(&data, protected)?;
            }
            if self.done {
                return Ok(());
            }
        }
        current.unchanged()?;
        for data in decoder.push(b"\n\n")? {
            self.event(&data, protected)?;
        }
        if self.done {
            Ok(())
        } else {
            Err(
                Error::http(502, "百度搭子上游流提前中断，未收到完整结束标记")
                    .with_code("dumate_stream_incomplete"),
            )
        }
    }

    pub fn fail(&self, error: &Error) -> Result<()> {
        if self.started {
            sdk::stream_event(&errors::event(error))?;
        }
        Ok(())
    }
}
