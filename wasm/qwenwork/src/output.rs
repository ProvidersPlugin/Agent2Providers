//! 千问办公 SSE 双层信封解码与 Chat 增量输出。首个有效内容前不发成功头，流内业务错正式返回。
//! 工具参数逐片透传，思考标签跨片拆解；不把断流或空响应伪装成 stop/[DONE]。
use super::{errors, thinking, util};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use std::collections::BTreeMap;

pub enum Event {
    Skip,
    Done,
    Chunk(Value),
}
pub fn parse(data: &str) -> Result<Event> {
    let data = data.trim();
    if data.is_empty() || matches!(data, "{}" | "null" | "[]") {
        return Ok(Event::Skip);
    }
    if data == "[DONE]" {
        return Ok(Event::Done);
    }
    let envelope: Value = serde_json::from_str(data).map_err(|_| Error::http(502, "千问办公 SSE 信封不是有效 JSON"))?;
    if errors::has_error(&envelope) {
        let status = util::integer(&envelope["statusCodeValue"])
            .and_then(|value| u16::try_from(value).ok())
            .unwrap_or(502);
        return Err(errors::failure(status, &envelope));
    }
    let inner = match envelope.get("body") {
        Some(Value::String(value)) if value.trim() == "[DONE]" => return Ok(Event::Done),
        Some(Value::String(value)) if matches!(value.trim(), "" | "{}" | "null" | "[]") => return Ok(Event::Skip),
        Some(Value::String(value)) => {
            serde_json::from_str(value).map_err(|_| Error::http(502, "千问办公 SSE 内层响应不是有效 JSON"))?
        }
        Some(Value::Null) => return Ok(Event::Skip),
        Some(value) => value.clone(),
        None if envelope.get("choices").is_some() || envelope.get("usage").is_some() => envelope,
        None if envelope["isQueued"] == false => return Ok(Event::Skip),
        None => return Err(Error::http(502, "千问办公 SSE 信封缺少 body")),
    };
    if inner.is_null() || inner.as_object().is_some_and(|fields| fields.is_empty())
        || inner.as_array().is_some_and(|rows| rows.is_empty()) {
        return Ok(Event::Skip);
    }
    if errors::has_error(&inner) {
        return Err(errors::failure(502, &inner));
    }
    if !inner.is_object() {
        return Err(Error::http(502, "千问办公 SSE 内层响应必须是对象"));
    }
    Ok(Event::Chunk(inner))
}
struct Choice {
    parser: Option<thinking::Parser>,
    finish: Option<String>,
    tools: BTreeMap<u64, (bool, bool)>,
    role_sent: bool,
}
pub struct Output {
    id: String,
    model: String,
    created: i64,
    thinking: bool,
    choices: BTreeMap<u64, Choice>,
    usage: Option<Value>,
    pub started: bool,
    productive: bool,
    pending: Vec<Value>,
    bytes: usize,
}
impl Output {
    pub fn new(model: &str, thinking: bool) -> Result<Self> {
        Ok(Self {
            id: format!("chatcmpl-{}", util::uuid()?.replace('-', "")),
            model: model.into(),
            created: util::now()? / 1000,
            thinking,
            choices: BTreeMap::new(),
            usage: None,
            started: false,
            productive: false,
            pending: Vec::new(),
            bytes: 0,
        })
    }
    fn frame(&self, index: u64, delta: Value, finish: Option<&str>) -> Value {
        json!({ "id": self.id, "object": "chat.completion.chunk", "created": self.created, "model": self.model,
            "choices": [{ "index": index, "delta": delta, "finish_reason": finish }] })
    }
    fn emit(&mut self, index: u64, delta: Value) -> Result<()> {
        let frame = self.frame(index, delta, None);
        if self.started {
            return sdk::stream_event(&frame);
        }
        self.bytes = self.bytes.saturating_add(frame.to_string().len());
        if self.bytes > 1024 * 1024 || self.pending.len() >= 128 {
            return Err(Error::http(502, "千问办公 仅返回过多控制数据，没有有效内容"));
        }
        self.pending.push(frame);
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        if !self.started {
            sdk::stream_head(200)?;
            self.started = true;
        }
        for value in self.pending.drain(..) {
            sdk::stream_event(&value)?;
        }
        self.bytes = 0;
        Ok(())
    }
    pub fn consume(&mut self, chunk: Value) -> Result<()> {
        if let Some(usage) = chunk.get("usage").filter(|value| value.is_object()) {
            self.usage = Some(usage.clone());
        }
        let Some(choices) = chunk["choices"].as_array() else {
            if chunk.get("usage").is_some_and(Value::is_object) {
                return Ok(());
            }
            return Err(Error::http(502, "千问办公 响应缺少 choices 或 usage"));
        };
        let mut output = Vec::new();
        for (fallback, choice) in choices.iter().enumerate() {
            let index = choice["index"].as_u64().unwrap_or(fallback as u64);
            if index >= 8 {
                return Err(Error::http(502, "千问办公 返回过多候选回答"));
            }
            let thinking = self.thinking;
            let state = self.choices.entry(index).or_insert_with(|| Choice {
                parser: thinking.then(thinking::Parser::default),
                finish: None,
                tools: BTreeMap::new(),
                role_sent: false,
            });
            if !state.role_sent {
                output.push((index, json!({ "role": "assistant", "content": "" })));
                state.role_sent = true;
            }
            let delta = &choice["delta"];
            if state.finish.is_some()
                && delta
                    .as_object()
                    .is_some_and(|fields| fields.iter().any(|(key, value)| key != "role" && util::truthy(value)))
            {
                return Err(Error::http(502, "千问办公 在结束后继续返回内容，未伪造成功"));
            }
            if let Some(finish) = choice["finish_reason"].as_str().filter(|value| !value.is_empty()) {
                if finish == "content_filter" {
                    return Err(errors::failure(400, &json!({ "code": "content_filter" })));
                }
                if state.finish.as_deref().is_some_and(|previous| previous != finish) {
                    return Err(Error::http(502, "千问办公 返回未知或冲突的结束原因，未伪造成功"));
                }
                state.finish = Some(finish.into());
            }
            if let Some(reasoning) = delta["reasoning_content"].as_str().filter(|value| !value.is_empty()) {
                let text = thinking::strip(reasoning);
                if !text.is_empty() {
                    output.push((index, json!({ "reasoning_content": text })));
                    self.productive = true;
                }
            }
            if let Some(content) = delta["content"].as_str().filter(|value| !value.is_empty()) {
                if let Some(parser) = state.parser.as_mut() {
                    for part in parser.push(content) {
                        let key = if part.reasoning { "reasoning_content" } else { "content" };
                        let mut delta = json!({});
                        delta[key] = json!(part.text);
                        output.push((index, delta));
                        self.productive = true;
                    }
                } else {
                    output.push((index, json!({ "content": content })));
                    self.productive = true;
                }
            }
            if let Some(calls) = delta["tool_calls"].as_array() {
                for call in calls {
                    let tool_index = call["index"].as_u64().unwrap_or(0);
                    if tool_index >= 128 {
                        return Err(Error::http(502, "千问办公 返回过多工具调用"));
                    }
                    let tool = state.tools.entry(tool_index).or_default();
                    tool.0 |= call["id"].as_str().is_some_and(|value| !value.is_empty());
                    tool.1 |= call["function"]["name"].as_str().is_some_and(|value| !value.is_empty());
                    let mut mapped = json!({ "index": tool_index, "type": "function", "function": {} });
                    if let Some(id) = call["id"].as_str() {
                        mapped["id"] = json!(id);
                    }
                    for key in ["name", "arguments"] {
                        if let Some(value) = call["function"].get(key) {
                            if !value.is_string() {
                                return Err(Error::http(502, "千问办公 工具调用增量格式无效"));
                            }
                            mapped["function"][key] = value.clone();
                        }
                    }
                    output.push((index, json!({ "tool_calls": [mapped] })));
                    self.productive = true;
                }
            }
        }
        for (index, delta) in output {
            self.emit(index, delta)?;
        }
        if self.productive && !self.started {
            self.flush()?;
        }
        Ok(())
    }
    pub fn finish(&mut self) -> Result<()> {
        if self.choices.is_empty()
            || self
                .choices
                .values()
                .any(|state| state.finish.is_none() || state.tools.values().any(|(id, name)| !*id || !*name))
        {
            return Err(Error::http(502, "千问办公 流缺少完整结束原因或工具标识，未伪造完成"));
        }
        let mut output = Vec::new();
        for (index, state) in &mut self.choices {
            if let Some(parser) = state.parser.as_mut() {
                for part in parser.finish() {
                    let mut delta = json!({});
                    delta[if part.reasoning { "reasoning_content" } else { "content" }] = json!(part.text);
                    output.push((*index, delta));
                    self.productive = true;
                }
            }
        }
        if !self.productive {
            return Err(Error::http(502, "千问办公 返回空回答，未把它伪装成成功"));
        }
        for (index, delta) in output {
            self.emit(index, delta)?;
        }
        self.flush()?;
        for (index, state) in &self.choices {
            let finish = if state.tools.is_empty() {
                state.finish.as_deref().unwrap_or("")
            } else {
                "tool_calls"
            };
            sdk::stream_event(&self.frame(*index, json!({}), Some(finish)))?;
        }
        if let Some(usage) = &self.usage {
            sdk::stream_event(
                &json!({ "id": self.id, "object": "chat.completion.chunk", "created": self.created,
                "model": self.model, "choices": [], "usage": usage }),
            )?;
        }
        sdk::stream_bytes(b"data: [DONE]\n\n")
    }
}
