//! assistant SSE → 标准 OpenAI Chat SSE，保留思考与 ACP 动作信封解包。
//! 真正的 [ERROR]: [LLM_ERROR] 不改成空成功；断流也不伪造正常 stop。
//! 原生没有估算 usage：这里只转交上游实际存在的 usage 对象，不造零值或猜 token 数。

use agent2api_wasm_sdk::{self as sdk, http, json, sse::SseDecoder, Error, Result, Value};

use super::{account, credentials::Credentials, errors};

const ENVELOPE_LIMIT: usize = 4 * 1024 * 1024;

pub struct Output {
    id: String,
    created: i64,
    model: String,
    decoder: SseDecoder,
    started: bool,
    finished: bool,
    buffered: Option<bool>,
    content: String,
    current_id: String,
    emitted_plain: bool,
    reasoning_seen: bool,
    usage: Option<Value>,
}

impl Output {
    pub fn new(model: &str) -> Self {
        let now = sdk::now();
        Self {
            id: format!(
                "chatcmpl-{}",
                now.saturating_mul(1000) % 1_000_000_000_000_000
            ),
            created: now / 1000,
            model: model.to_string(),
            decoder: SseDecoder::default(),
            started: false,
            finished: false,
            buffered: None,
            content: String::new(),
            current_id: String::new(),
            emitted_plain: false,
            reasoning_seen: false,
            usage: None,
        }
    }

    fn head(&mut self) -> Result<()> {
        if !self.started {
            sdk::stream_head(200)?;
            self.started = true;
        }
        Ok(())
    }

    fn chunk(&mut self, text: &str, reasoning: bool, finish: Option<&str>) -> Result<()> {
        self.head()?;
        let mut delta = json!({});
        if !text.is_empty() {
            delta[if reasoning {
                "reasoning_content"
            } else {
                "content"
            }] = json!(text);
        }
        sdk::stream_event(&json!({
            "id": self.id, "object": "chat.completion.chunk", "created": self.created, "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }]
        }))
    }

    fn text(&mut self, delta: &str) -> Result<()> {
        if self.buffered == Some(false) {
            self.emitted_plain |= !delta.is_empty();
            return self.chunk(delta, false, None);
        }
        if self.content.len().saturating_add(delta.len()) > ENVELOPE_LIMIT {
            return Err(Error::http(
                502,
                "OrcaTerm 动作信封超过 4 MiB，未静默截断内容",
            ));
        }
        self.content.push_str(delta);
        if self.buffered.is_none() {
            let head = self.content.trim_start();
            if head.is_empty() || "```".starts_with(head) && head.len() < 3 {
                return Ok(());
            }
            let envelope = head.starts_with('{') || head.starts_with("```");
            self.buffered = Some(envelope);
            if !envelope {
                let text = std::mem::take(&mut self.content);
                self.emitted_plain |= !text.is_empty();
                return self.chunk(&text, false, None);
            }
        }
        Ok(())
    }

    fn flush_content(&mut self, finish: Option<&str>) -> Result<()> {
        if self.buffered != Some(false) {
            let raw = std::mem::take(&mut self.content);
            if !raw.is_empty() {
                if raw.trim_start().starts_with("{ERROR}:") {
                    return Err(Error::http(
                        502,
                        "OrcaTerm 上游返回动作错误，未伪装成正常正文",
                    ));
                }
                if !self.reasoning_seen {
                    if let Some(reasoning) = envelope_reasoning(&raw) {
                        self.chunk(&reasoning, true, None)?;
                    }
                }
                let text = unwrap_envelope(&raw).unwrap_or(raw);
                self.emitted_plain |= !text.is_empty();
                return self.chunk(&text, false, finish);
            }
        }
        if finish.is_some() {
            self.chunk("", false, finish)?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.flush_content(Some("stop"))?;
        if let Some(usage) = self.usage.take() {
            self.head()?;
            sdk::stream_event(&json!({ "id": self.id, "object": "chat.completion.chunk",
                "created": self.created, "model": self.model, "choices": [], "usage": usage }))?;
        }
        sdk::stream_bytes(b"data: [DONE]\n\n")?;
        self.finished = true;
        Ok(())
    }

    fn event(&mut self, payload: &str, token: &str) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        if payload.trim() == "[DONE]" {
            return self.finish();
        }
        if let Some(message) = payload.trim_start().strip_prefix("[ERROR]:") {
            return Err(errors::upstream(
                200,
                &json!({ "message": message.trim() }),
                &[token],
            ));
        }
        if payload.trim_start().starts_with("[ABORTED]") {
            return Err(Error::http(502, "OrcaTerm 上游已中止生成，未当作完成"));
        }
        let value = serde_json::from_str::<Value>(payload)
            .map_err(|_| Error::http(502, "OrcaTerm 流式事件不是有效 JSON"))?;
        if errors::business_failed(&value) {
            return Err(errors::upstream(200, &value, &[token]));
        }
        if let Some(usage) = value.get("usage").filter(|usage| usage.is_object()) {
            self.usage = Some(usage.clone());
        }
        let id = value["id"].as_str().unwrap_or("");
        if id.starts_with("heartbeat-") {
            return Ok(());
        }
        if let Some(removed) = id.strip_prefix("remove-") {
            if removed == self.current_id {
                if self.emitted_plain {
                    return Err(Error::http(
                        502,
                        "OrcaTerm 上游撤回了已发送正文，未拼接改写回答",
                    ));
                }
                self.content.clear();
                self.buffered = None;
            }
            return Ok(());
        }
        if value["type"] != "ai" {
            return Ok(());
        }
        if !id.is_empty() && id != self.current_id {
            if !self.current_id.is_empty() {
                self.flush_content(None)?;
                self.buffered = None;
            }
            self.current_id = id.to_string();
            self.emitted_plain = false;
            self.reasoning_seen = false;
        }
        if value["reasoning"] != true {
            if let Some(text) = value["reasoningContent"]
                .as_str()
                .filter(|text| !text.is_empty())
            {
                self.reasoning_seen = true;
                self.chunk(text, true, None)?;
            }
        }
        let reasoning = value["reasoning"].as_bool().unwrap_or(false);
        let key = if reasoning {
            "reasoningContent"
        } else {
            "content"
        };
        if let Some(text) = value[key].as_str().filter(|text| !text.is_empty()) {
            if reasoning {
                self.reasoning_seen = true;
                self.chunk(text, true, None)?;
            } else {
                self.text(text)?;
            }
        }
        Ok(())
    }

    fn consume(&mut self, mut response: http::Response, credentials: &Credentials) -> Result<()> {
        while let Some(bytes) = response.read()? {
            account::validate_current(credentials)?;
            for payload in self.decoder.push(&bytes)? {
                self.event(&payload, &credentials.access_token)?;
            }
            if self.finished {
                return Ok(());
            }
        }
        account::validate_current(credentials)?;
        for payload in self.decoder.push(b"\n\n")? {
            self.event(&payload, &credentials.access_token)?;
        }
        if self.finished {
            Ok(())
        } else {
            Err(Error::http(
                502,
                "OrcaTerm 上游流式传输提前结束，未收到完整结束标记",
            ))
        }
    }

    pub fn forward(mut self, response: http::Response, credentials: &Credentials) -> Result<()> {
        let result = self.consume(response, credentials);
        if let Err(error) = result {
            // 故障时仍交付已经收到的正文，但绝不能发 stop/[DONE] 把错误算成成功。
            if self.started
                && !self.content.is_empty()
                && error.code.as_deref() != Some("account_identity_changed")
                && error.code.as_deref() != Some("account_credentials_changed")
            {
                self.flush_content(None)?;
            }
            if self.started {
                sdk::stream_event(&errors::event(&error))?;
            }
            return Err(error);
        }
        Ok(())
    }
}

fn envelope_reasoning(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let inner = if let Some(fence) = trimmed.strip_prefix("```") {
        fence
            .split_once('\n')?
            .1
            .trim_end()
            .strip_suffix("```")?
            .trim()
    } else {
        trimmed
    };
    let value: Value = serde_json::from_str(inner).ok()?;
    value["action"].as_str()?;
    value["thinking"]
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn unwrap_envelope(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let inner = if trimmed.starts_with("```") {
        trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim()
    } else {
        trimmed
    };
    let value: Value = serde_json::from_str(inner).ok()?;
    let object = value.as_object()?;
    let action = object.get("action").and_then(Value::as_str)?;
    let text = match action {
        "completion" => object.get("taskCompletion").and_then(Value::as_str),
        "default" => object.get("content").and_then(Value::as_str),
        _ => ["taskCompletion", "content", "question"]
            .iter()
            .find_map(|key| object.get(*key).and_then(Value::as_str)),
    }?
    .trim();
    (!text.is_empty()).then(|| text.to_string())
}
