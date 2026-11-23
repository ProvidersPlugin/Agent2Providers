//! Trae SSE保留event名称与多行data；通用SDK解码器不保留event，不能用于本家error/done门。
//! 大小受限，坏JSON明确失败；只处理本家别名，不复制通用协议转换。

use super::{errors, util};
use agent2api_wasm_sdk::{Error, Result, Value};

pub enum Event {
    Output {
        text: String,
        reasoning: String,
        tools: Option<Value>,
    },
    Usage(Value),
    Done {
        reason: String,
        usage: Option<Value>,
    },
    Error(Error),
    Other,
}

fn finish_reason(value: &Value) -> String {
    let text =
        util::text(value, &["finish_reason", "stop_reason", "finishReason"]).to_ascii_lowercase();
    match text.as_str() {
        "max_tokens" | "max_output_tokens" | "max_length" => "length".into(),
        "sensitive" => "content_filter".into(),
        "" => "stop".into(),
        _ => text,
    }
}

fn event_name(value: &str) -> String {
    let mut out = String::new();
    let mut lower_or_digit = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            if ch.is_ascii_uppercase() && lower_or_digit {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
            lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        } else {
            if !out.is_empty() && !out.ends_with('_') {
                out.push('_');
            }
            lower_or_digit = false;
        }
    }
    out.trim_matches('_').into()
}

fn parse(event: &str, data: &str) -> Result<Event> {
    let event = event_name(event);
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return Ok(if event == "error" {
            Event::Error(errors::upstream(502, &Value::Null))
        } else if matches!(event.as_str(), "done" | "response_done" | "stream_done")
            || data == "[DONE]"
        {
            Event::Done {
                reason: "stop".into(),
                usage: None,
            }
        } else {
            Event::Other
        });
    }
    let mut parsed: Value =
        serde_json::from_str(data).map_err(|_| Error::http(502, "Trae 流式事件不是有效 JSON"))?;
    if let Some(inner) = parsed.as_str() {
        parsed =
            serde_json::from_str(inner).map_err(|_| Error::http(502, "Trae 嵌套流式事件无效"))?;
    }
    if !parsed.is_object() {
        return Err(Error::http(502, "Trae 流式事件不是对象"));
    }
    let kind = if event.is_empty() {
        event_name(&util::text(&parsed, &["event", "type"]))
    } else {
        event
    };
    if kind == "error" || errors::failed(&parsed) {
        return Ok(Event::Error(errors::upstream(200, &parsed)));
    }
    Ok(match kind.as_str() {
        "" | "output" | "message" | "delta" | "content" | "text" | "answer" | "chunk"
        | "output_delta" | "message_delta" => {
            let first = |fields: &[Option<&Value>]| {
                fields
                    .iter()
                    .flatten()
                    .filter_map(|v| v.as_str())
                    .find(|v| !v.is_empty())
                    .unwrap_or("")
                    .to_string()
            };
            Event::Output {
                text: first(&[
                    parsed.get("response"),
                    parsed.get("content"),
                    parsed.pointer("/delta/content"),
                    parsed.get("delta"),
                    parsed.get("text"),
                    parsed.get("message"),
                ]),
                reasoning: first(&[
                    parsed.get("reasoning_content"),
                    parsed.get("reasoning"),
                    parsed.pointer("/delta/reasoning_content"),
                    parsed.pointer("/delta/reasoning"),
                ]),
                tools: parsed
                    .get("tool_calls")
                    .or_else(|| parsed.pointer("/delta/tool_calls"))
                    .filter(|v| !v.is_null())
                    .cloned(),
            }
        }
        "token_usage" => Event::Usage(parsed.get("usage").cloned().unwrap_or(parsed)),
        "done" | "response_done" | "stream_done" => Event::Done {
            reason: finish_reason(&parsed),
            usage: parsed
                .get("usage")
                .filter(|value| value.is_object())
                .cloned(),
        },
        _ => Event::Other,
    })
}

#[derive(Default)]
pub struct Decoder {
    line: Vec<u8>,
    event: String,
    data: String,
    skip_lf: bool,
    bytes: usize,
}

impl Decoder {
    fn end_line(&mut self, out: &mut Vec<Event>) -> Result<()> {
        let bytes = std::mem::take(&mut self.line);
        let line =
            std::str::from_utf8(&bytes).map_err(|_| Error::http(502, "Trae SSE 不是有效 UTF-8"))?;
        if line.is_empty() {
            if !self.event.is_empty() || !self.data.is_empty() {
                out.push(parse(&self.event, &self.data).unwrap_or_else(Event::Error));
            }
            self.event.clear();
            self.data.clear();
            self.bytes = 0;
        } else if let Some(event) = line.strip_prefix("event:") {
            self.event = event.trim().into();
        } else if let Some(data) = line.strip_prefix("data:") {
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
        }
        Ok(())
    }

    pub fn push(&mut self, input: &[u8]) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        for byte in input {
            if self.skip_lf {
                self.skip_lf = false;
                if *byte == b'\n' {
                    continue;
                }
            }
            self.bytes = self.bytes.saturating_add(1);
            if self.bytes > 1024 * 1024 {
                events.push(Event::Error(Error::http(
                    502,
                    "Trae 单个流式事件超过大小限制",
                )));
                self.line.clear();
                self.event.clear();
                self.data.clear();
                self.bytes = 0;
                break;
            }
            if matches!(*byte, b'\r' | b'\n') {
                if let Err(error) = self.end_line(&mut events) {
                    events.push(Event::Error(error));
                    break;
                }
                self.skip_lf = *byte == b'\r';
            } else {
                self.line.push(*byte);
            }
            // 正常终态之后的同分片噪音不覆盖已确认完成。
            if matches!(events.last(), Some(Event::Done { .. })) {
                break;
            }
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        if !self.line.is_empty() {
            self.end_line(&mut events)?;
        }
        self.end_line(&mut events)?;
        Ok(events)
    }
}
