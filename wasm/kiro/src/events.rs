//! AWS 二进制帧 -> 标准Chat增量，不依赖通用SSE/Gemini转换器。
//! 原生以干净EOF结束；保留此语义，但无内容空流、坏CRC、残帧、坏JSON和中途异常不报成功。
//! tokenUsage与窗口占用是两件事，后者仅作独立观测字段，不换算为token数量。

use crate::{errors, eventstream, thinking, token_usage::Usage, tool_stream::Calls};
use agent2api_wasm_sdk::{http, json, Error, Result, Value};
use std::collections::{HashMap, VecDeque};

pub enum Event {
    Delta(Value),
    Keepalive,
    Stop {
        reason: String,
        usage: Option<Value>,
        context: Option<f64>,
    },
}
pub struct Decoder {
    reader: eventstream::Reader,
    think: thinking::Parser,
    split: bool,
    pending: VecDeque<Event>,
    usage: Usage,
    calls: Calls,
    context: Option<f64>,
    reason: String,
    visible: bool,
    finished: bool,
    names: HashMap<String, String>,
}
fn bad(message: &str) -> Error {
    Error::http(502, message).with_code("kiro_eventstream_invalid")
}
impl Decoder {
    pub fn new(response: http::Response, split: bool, names: HashMap<String, String>) -> Self {
        Self {
            reader: eventstream::Reader::new(response),
            think: thinking::Parser::default(),
            split,
            pending: VecDeque::new(),
            usage: Usage::default(),
            calls: Calls::default(),
            context: None,
            reason: String::new(),
            visible: false,
            finished: false,
            names,
        }
    }
    fn parts(&mut self, parts: Vec<thinking::Part>) {
        for part in parts {
            let (field, text) = match part {
                thinking::Part::Text(text) => ("content", text),
                thinking::Part::Think(text) => ("reasoning_content", text),
            };
            if !text.is_empty() {
                self.visible = true;
                self.pending.push_back(Event::Delta(json!({ field: text })));
            }
        }
    }
    fn finish(&mut self) -> Result<()> {
        if self.split {
            let parts = self.think.end();
            self.parts(parts);
        }
        let reason = self.reason.to_ascii_lowercase();
        if reason == "model_context_window_exceeded" && !self.visible && self.calls.len() == 0 {
            return Err(Error::http(
                400,
                format!("{}MODEL_CONTEXT_WINDOW_EXCEEDED", errors::TOO_LONG),
            )
            .with_code("context_length_exceeded"));
        }
        if matches!(reason.as_str(), "error" | "failed") {
            return Err(bad("Kiro 以失败状态结束本次回复"));
        }
        if !matches!(
            reason.as_str(),
            "" | "end_turn"
                | "stop"
                | "stop_sequence"
                | "tool_use"
                | "tool_calls"
                | "model_context_window_exceeded"
                | "max_tokens"
                | "max_output_tokens"
                | "content_filtered"
                | "content_filter"
        ) {
            return Err(bad("Kiro 返回尚未支持的终态，未伪造正常完成"));
        }
        let full = matches!(
            reason.as_str(),
            "model_context_window_exceeded" | "max_tokens" | "max_output_tokens"
        );
        let blocked = matches!(reason.as_str(), "content_filtered" | "content_filter");
        if blocked && !self.visible {
            return Err(
                Error::http(400, "Kiro内容策略拒绝了本次请求").with_code("kiro_content_blocked")
            );
        }
        if !self.visible && self.calls.len() == 0 && !full && !blocked {
            return Err(bad("Kiro 事件流未返回正文、思考或有效工具调用"));
        }
        if matches!(reason.as_str(), "tool_use" | "tool_calls") && self.calls.len() == 0 {
            return Err(bad("Kiro 以工具调用结束，但没有返回有效工具"));
        }
        let mut calls = if blocked {
            Vec::new()
        } else {
            self.calls.finish()?
        };
        // 先验证本次所有工具，再交付任何一个；未知工具不会在前几个已经交付后才报错。
        for call in &mut calls {
            let name = call["function"]["name"].as_str().unwrap_or("");
            let original = self.names.get(name).ok_or_else(|| {
                Error::http(502, "Kiro返回了未声明的工具，未交付调用")
                    .with_code("kiro_tool_invalid")
            })?;
            call["function"]["name"] = json!(original);
        }
        for call in calls {
            self.pending
                .push_back(Event::Delta(json!({ "tool_calls": [call] })));
        }
        let reason = if blocked {
            "content_filter"
        } else if self.calls.len() > 0 {
            "tool_calls"
        } else if full {
            "length"
        } else {
            "stop"
        };
        self.pending.push_back(Event::Stop {
            reason: reason.into(),
            usage: self.usage.value(),
            context: self.context,
        });
        self.finished = true;
        Ok(())
    }
    pub fn usage(&self) -> Option<Value> {
        self.usage.value()
    }
    pub fn next(&mut self, secrets: &[&str]) -> Result<Option<Event>> {
        let started = crate::util::now()?;
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            if self.finished {
                return Ok(None);
            }
            let Some(frame) = self.reader.next()? else {
                self.finish()?;
                continue;
            };
            if frame.payload.len() > 8 * 1024 * 1024 {
                return Err(bad("Kiro 单条JSON事件超过上限"));
            }
            if frame.error() {
                let value = serde_json::from_slice::<Value>(&frame.payload)
                    .unwrap_or_else(|_| json!(std::str::from_utf8(&frame.payload).unwrap_or("")));
                return Err(errors::exception(frame.error_kind(), &value, secrets));
            }
            let value: Value = serde_json::from_slice(&frame.payload)
                .map_err(|_| bad("Kiro 事件正文不是有效JSON"))?;
            if !value.is_object() {
                return Err(bad("Kiro 事件正文不是对象"));
            }
            match frame.event_type.as_str() {
                "assistantResponseEvent" => {
                    let text = match value.get("content") {
                        Some(Value::String(text)) => text.as_str(),
                        None | Some(Value::Null) => "",
                        _ => return Err(bad("Kiro 文本事件格式无效")),
                    };
                    if !text.is_empty() {
                        if self.split {
                            let parts = self.think.feed(text)?;
                            self.parts(parts);
                        } else {
                            self.visible = true;
                            self.pending
                                .push_back(Event::Delta(json!({ "content": text })));
                        }
                    }
                }
                "reasoningContentEvent" => {
                    if value
                        .get("text")
                        .is_some_and(|value| !value.is_null() && !value.is_string())
                    {
                        return Err(bad("Kiro思考事件文本格式无效"));
                    }
                    if let Some(text) = value
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                    {
                        self.visible = true;
                        self.pending
                            .push_back(Event::Delta(json!({ "reasoning_content": text })));
                    }
                }
                "toolUseEvent" => {
                    if self.split {
                        let parts = self.think.end();
                        self.parts(parts);
                    }
                    self.calls.push(&value)?;
                }
                "metadataEvent" | "messageMetadataEvent" => {
                    if let Some(reason) = value.get("stopReason").filter(|reason| !reason.is_null())
                    {
                        let reason = reason
                            .as_str()
                            .filter(|reason| {
                                reason.len() <= 128 && !reason.chars().any(char::is_control)
                            })
                            .ok_or_else(|| bad("Kiro 停止原因格式无效"))?;
                        if !reason.is_empty() {
                            self.reason = reason.into();
                        }
                    }
                    if let Some(usage) = value.get("tokenUsage").filter(|usage| !usage.is_null()) {
                        self.usage.read(usage)?;
                        self.context(usage)?;
                    }
                    self.context(&value)?;
                }
                "meteringEvent" => self.usage.metering(&value)?,
                "contextUsageEvent" => self.context(&value)?,
                // 原生的未知元事件不转成文本；不从元信息猜工具、token或结束标记。
                _ => {}
            }
            if self.pending.is_empty()
                && self.calls.len() > 0
                && crate::util::now()?.saturating_sub(started) >= 10_000
            {
                // 大工具参数仍在生成时仅发 SSE 注释，不交付未经完整校验的工具片段。
                return Ok(Some(Event::Keepalive));
            }
        }
    }
    fn context(&mut self, value: &Value) -> Result<()> {
        if let Some(value) = value
            .get("contextUsagePercentage")
            .filter(|value| !value.is_null())
        {
            self.context = Some(
                value
                    .as_f64()
                    .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
                    .ok_or_else(|| bad("Kiro 上下文占用读数无效"))?,
            );
        }
        Ok(())
    }
}
