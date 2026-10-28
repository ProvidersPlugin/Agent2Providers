//! 灵犀直连 SSE → 标准 chat SSE 的翻译。
//!
//! ── 上游帧长什么样（2026-10-28 实测）───────────────────────
//! ```text
//! data:{"code":"Success","message":"","usage":{…},
//!       "choices":[{"index":0,"finish_reason":"","text":"1",
//!                   "reasoning_content":"…","tool_calls":[…]}]}
//! ```
//! 与 OpenAI 的三处差别：
//!   1. 正文在 `choices[].text`（**不是** `delta.content`），思考在
//!      `choices[].reasoning_content`；
//!   2. `usage` 每帧都有（大多为 0，最后一帧才是真值）；
//!   3. 流里**没有** `[DONE]`，靠 `finish_reason` 收尾。
//!
//! 本机状态机把它们折成标准 chat SSE（`delta.content` / `delta.tool_calls` /
//! 收尾帧 / usage 帧 / `[DONE]`）。工具调用是**增量**：首帧带 `id`/`name`，
//! 后续帧只有 `arguments` 片段 —— 原样按 `index` 下发，参数由客户端拼接。

use bytes::Bytes;
use serde_json::{json, Map, Value};

/// 一次流的状态机。
pub struct Translator {
    buffer: Vec<u8>,
    id: String,
    created: i64,
    model: String,
    started: bool,
    saw_finish: bool,
    finished: bool,
    finish_reason: Option<String>,
    usage: Option<Value>,
}

impl Translator {
    pub fn new(model: &str) -> Self {
        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_secs() as i64)
            .unwrap_or(0);
        Self {
            buffer: Vec::new(),
            id: format!("chatcmpl-lingxi-{created}-{}", std::process::id()),
            created,
            model: model.to_string(),
            started: false,
            saw_finish: false,
            finished: false,
            finish_reason: None,
            usage: None,
        }
    }

    /// 吃一段上游字节，吐出要下发的 chat SSE 帧。
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Bytes>, String> {
        let mut out = Vec::new();
        self.buffer.extend_from_slice(chunk);
        let mut start = 0usize;
        while let Some(offset) = self.buffer[start..].iter().position(|byte| *byte == b'\n') {
            let end = start + offset;
            let line = String::from_utf8_lossy(&self.buffer[start..end]).to_string();
            start = end + 1;
            out.extend(self.consume_line(&line)?);
            if self.finished {
                break;
            }
        }
        self.buffer.drain(..start);
        Ok(out)
    }

    /// 上游 EOF：没见到 finish_reason 按「截断」报错，否则补 usage 帧与 `[DONE]`。
    pub fn finish(&mut self) -> Result<Vec<Bytes>, String> {
        if self.finished {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        if !self.buffer.is_empty() {
            let line = String::from_utf8_lossy(&self.buffer).to_string();
            self.buffer.clear();
            out.extend(self.consume_line(&line)?);
        }
        if self.finished {
            return Ok(out);
        }
        if !self.saw_finish {
            self.finished = true;
            return Err("灵犀上游流在完成事件之前结束（响应被截断）".to_string());
        }
        self.finished = true;
        if let Some(usage) = self.usage.clone() {
            out.push(chat_frame(&json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.model,
                "choices": [],
                "usage": usage,
            })));
        }
        out.push(Bytes::from_static(b"data: [DONE]\n\n"));
        Ok(out)
    }

    fn consume_line(&mut self, raw: &str) -> Result<Vec<Bytes>, String> {
        if self.finished {
            return Ok(Vec::new());
        }
        let line = raw.trim();
        if line.is_empty() || line.starts_with(':') || line.starts_with("event:") {
            return Ok(Vec::new());
        }
        let payload = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
        if payload.is_empty() || payload == "[DONE]" {
            if payload == "[DONE]" {
                self.saw_finish = true;
            }
            return Ok(Vec::new());
        }
        let value: Value = match serde_json::from_str(payload) {
            Ok(value) => value,
            // 非 JSON 行（上游偶发进度行）忽略，但留一条日志
            Err(_) => {
                crate::logging::log("[Lingxi]", &format!("忽略非 JSON 的 SSE 行：{}", payload.chars().take(120).collect::<String>()));
                return Ok(Vec::new());
            }
        };
        self.consume_frame(&value)
    }

    fn consume_frame(&mut self, frame: &Value) -> Result<Vec<Bytes>, String> {
        if let Some(code) = frame.get("code").and_then(Value::as_str) {
            if !code.eq_ignore_ascii_case("Success") {
                let message = frame
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .unwrap_or("上游返回错误");
                self.finished = true;
                return Err(format!("灵犀上游返回 {code}：{message}"));
            }
        }
        if let Some(usage) = frame.get("usage").filter(|usage| usage.is_object()) {
            let total = usage.get("total_tokens").and_then(Value::as_i64).unwrap_or(0);
            if total > 0 {
                self.usage = Some(usage.clone());
            }
        }

        let mut out = Vec::new();
        let choices = frame.get("choices").and_then(Value::as_array).cloned().unwrap_or_default();
        for choice in choices {
            let text = choice.get("text").and_then(Value::as_str).unwrap_or("");
            let reasoning = choice.get("reasoning_content").and_then(Value::as_str).unwrap_or("");
            let tool_calls = choice.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default();

            if !text.is_empty() || !reasoning.is_empty() || !tool_calls.is_empty() {
                self.start(&mut out);
                let mut delta = Map::new();
                if !text.is_empty() {
                    delta.insert("content".to_string(), Value::String(text.to_string()));
                }
                if !reasoning.is_empty() {
                    delta.insert("reasoning_content".to_string(), Value::String(reasoning.to_string()));
                }
                if !tool_calls.is_empty() {
                    let calls: Vec<Value> = tool_calls.iter().enumerate().map(|(index, call)| {
                        let mut obj = Map::new();
                        obj.insert("index".to_string(), Value::from(call.get("index").and_then(Value::as_i64).unwrap_or(index as i64)));
                        let id = call.get("id").and_then(Value::as_str).unwrap_or("");
                        if !id.is_empty() {
                            obj.insert("id".to_string(), Value::String(id.to_string()));
                            obj.insert("type".to_string(), Value::String("function".to_string()));
                        }
                        let function = call.get("function").cloned().unwrap_or(Value::Null);
                        let mut fn_obj = Map::new();
                        if let Some(name) = function.get("name").and_then(Value::as_str).filter(|name| !name.is_empty()) {
                            fn_obj.insert("name".to_string(), Value::String(name.to_string()));
                        }
                        if let Some(arguments) = function.get("arguments").and_then(Value::as_str).filter(|args| !args.is_empty()) {
                            fn_obj.insert("arguments".to_string(), Value::String(arguments.to_string()));
                        }
                        obj.insert("function".to_string(), Value::Object(fn_obj));
                        Value::Object(obj)
                    }).collect();
                    delta.insert("tool_calls".to_string(), Value::Array(calls));
                }
                out.push(self.delta_frame(Value::Object(delta)));
            }

            if let Some(reason) = choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
            {
                self.saw_finish = true;
                self.finish_reason = Some(reason.to_string());
            }
        }

        if self.saw_finish && !self.finished {
            self.finished = true;
            let reason = self.finish_reason.clone().unwrap_or_else(|| "stop".to_string());
            out.push(chat_frame(&json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.model,
                "choices": [{ "index": 0, "delta": {}, "finish_reason": reason }],
            })));
            if let Some(usage) = self.usage.clone() {
                out.push(chat_frame(&json!({
                    "id": self.id,
                    "object": "chat.completion.chunk",
                    "created": self.created,
                    "model": self.model,
                    "choices": [],
                    "usage": usage,
                })));
            }
            out.push(Bytes::from_static(b"data: [DONE]\n\n"));
        }
        Ok(out)
    }

    /// 首个内容帧之前补一个 role 帧（部分客户端依赖它）。
    fn start(&mut self, out: &mut Vec<Bytes>) {
        if self.started {
            return;
        }
        self.started = true;
        out.push(self.delta_frame(json!({ "role": "assistant", "content": "" })));
    }

    fn delta_frame(&self, delta: Value) -> Bytes {
        chat_frame(&json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": Value::Null }],
        }))
    }
}

fn chat_frame(value: &Value) -> Bytes {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    Bytes::from(format!("data: {text}\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(frames: &[Bytes]) -> String {
        frames.iter().map(|frame| String::from_utf8_lossy(frame).to_string()).collect()
    }

    fn line(frame: Value) -> String {
        format!("data:{}\n", frame)
    }

    #[test]
    fn maps_text_reasoning_and_finish() {
        let mut machine = Translator::new("GLM-5.3");
        let frames = machine
            .push(
                line(json!({
                    "code": "Success",
                    "usage": { "total_tokens": 0 },
                    "choices": [{ "index": 0, "text": "", "reasoning_content": "想", "finish_reason": "" }]
                }))
                .as_bytes(),
            )
            .expect("push");
        let body = text(&frames);
        assert!(body.contains("\"role\":\"assistant\""));
        assert!(body.contains("\"reasoning_content\":\"想\""));
        let frames = machine
            .push(
                line(json!({
                    "code": "Success",
                    "usage": { "prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4 },
                    "choices": [{ "index": 0, "text": "答", "reasoning_content": "", "finish_reason": "stop" }]
                }))
                .as_bytes(),
            )
            .expect("push");
        let body = text(&frames);
        assert!(body.contains("\"content\":\"答\""));
        assert!(body.contains("\"finish_reason\":\"stop\""));
        assert!(body.contains("\"total_tokens\":4"));
        assert!(body.ends_with("data: [DONE]\n\n"));
        assert!(machine.finish().expect("finish").is_empty());
    }

    #[test]
    fn maps_tool_call_deltas() {
        let mut machine = Translator::new("GLM-5.3");
        let frames = machine
            .push(
                line(json!({
                    "code": "Success",
                    "choices": [{
                        "index": 0,
                        "text": "",
                        "finish_reason": "",
                        "tool_calls": [{ "type": "function", "id": "call_1", "index": 0,
                                         "function": { "name": "get_weather", "arguments": "" } }]
                    }]
                }))
                .as_bytes(),
            )
            .expect("push");
        let body = text(&frames);
        assert!(body.contains("\"tool_calls\""));
        assert!(body.contains("\"name\":\"get_weather\""));
        assert!(body.contains("\"id\":\"call_1\""));
        let frames = machine
            .push(
                line(json!({
                    "code": "Success",
                    "choices": [{ "index": 0, "text": "", "finish_reason": "tool_calls",
                                  "tool_calls": [{ "type": "", "id": "", "index": 0,
                                                   "function": { "name": "", "arguments": "{\"city\": \"北京\"}" } }] }]
                }))
                .as_bytes(),
            )
            .expect("push");
        let body = text(&frames);
        assert!(body.contains("\"arguments\":\"{\\\"city\\\": \\\"北京\\\"}\""));
        assert!(body.contains("\"finish_reason\":\"tool_calls\""));
    }

    #[test]
    fn truncated_stream_is_an_error() {
        let mut machine = Translator::new("GLM-5.3");
        machine
            .push(line(json!({ "code": "Success", "choices": [{ "text": "半句", "finish_reason": "" }] })).as_bytes())
            .expect("push");
        assert!(machine.finish().is_err());
    }

    #[test]
    fn upstream_error_code_becomes_err() {
        let mut machine = Translator::new("GLM-5.3");
        let error = machine
            .push(line(json!({ "code": "UserUnauthorized", "message": "登录失效" })).as_bytes())
            .expect_err("必须报错");
        assert!(error.contains("UserUnauthorized"));
    }
}
