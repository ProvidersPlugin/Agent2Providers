//! Kiro 事件 → 中立的增量事件（文本 / 思考 / 工具调用 / 用量）。
//!
//! ── 思考标签 ────────────────────────────────────────────────
//! 一部分模型（Claude）把思考写在回复正文的 `<thinking>…</thinking>` 里。
//! 这个解析器负责把两段切开，**跨 chunk** 也能正确切：标签可能被拆在
//! 两次增量里（`<thin` + `king>`），所以未闭合的后缀要暂存到下一次。
//!
//! ── 用量 ────────────────────────────────────────────────────
//! Kiro 通常只给 `contextUsagePercentage`（上下文用了百分之几），不给 token
//! 数。此时按「窗口大小 × 百分比」估算输入，输出按正文字符数 / 4 估算；
//! 两者都只是**估算**，但比报 0 有用（报表能看出量级）。

use std::collections::VecDeque;

use crate::logging;

use super::eventstream::FrameReader;

/// 一条增量事件。
pub enum Event {
    Text(String),
    Think(String),
    ToolStart { id: String, name: String },
    ToolArgs(String),
    Stop { reason: String, usage: Usage },
    Error { status: Option<u16>, text: String },
}

/// 一次回复的用量（OpenAI chat 的口径）。
#[derive(Clone, Copy, Default)]
pub struct Usage {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    /// Kiro 真的给了 tokenUsage（否则输入是估算的）
    pub reported: bool,
}

/// 流结束时的用量（不含估算）。
pub struct Decoder {
    reader: FrameReader,
    think: ThinkParser,
    pending: VecDeque<Event>,
    usage: Usage,
    percentage: f64,
    said: usize,
    tools: usize,
    current_tool: Option<String>,
    stop_reason: String,
    failed: Option<String>,
    finished: bool,
    model: String,
    account: String,
    split_thinking: bool,
}

impl Decoder {
    pub fn new(
        response: reqwest::Response,
        model: &str,
        account: &str,
        split_thinking: bool,
    ) -> Self {
        Self {
            reader: FrameReader::new(response),
            think: ThinkParser::default(),
            pending: VecDeque::new(),
            usage: Usage::default(),
            percentage: 0.0,
            said: 0,
            tools: 0,
            current_tool: None,
            stop_reason: String::new(),
            failed: None,
            finished: false,
            model: model.to_string(),
            account: account.to_string(),
            split_thinking,
        }
    }

    /// 读下一个事件（流结束返回 `None`）。
    pub async fn next(&mut self) -> Result<Option<Event>, String> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            if self.finished {
                return Ok(None);
            }
            let frame = match self.reader.next().await {
                Ok(Some(frame)) => frame,
                Ok(None) => {
                    self.finish();
                    continue;
                }
                Err(error) => {
                    self.failed = Some(error);
                    self.finish();
                    continue;
                }
            };
            if frame.is_error() {
                let text = super::errors::exception_text(
                    &frame.error_kind(),
                    &String::from_utf8_lossy(&frame.payload),
                );
                logging::verbose("kiro", &format!("事件流错误：{text}"));
                self.failed = Some(text);
                self.finish();
                continue;
            }
            let payload = String::from_utf8_lossy(&frame.payload).to_string();
            let value: serde_json::Value = match serde_json::from_str(&payload) {
                Ok(value) => value,
                Err(_) => continue,
            };
            match frame.event_type.as_str() {
                "assistantResponseEvent" => {
                    let text = value
                        .get("content")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    if text.is_empty() {
                        continue;
                    }
                    self.said += text.chars().count();
                    if self.split_thinking {
                        let produced = self.think.feed(text);
                        self.pending.extend(produced);
                    } else {
                        self.pending.push_back(Event::Text(text.to_string()));
                    }
                }
                "reasoningContentEvent" => {
                    if let Some(text) = value.get("text").and_then(|value| value.as_str()) {
                        if !text.is_empty() {
                            self.said += text.chars().count();
                            self.pending.push_back(Event::Think(text.to_string()));
                        }
                    }
                }
                "toolUseEvent" => self.tool_event(&value),
                "metadataEvent" | "messageMetadataEvent" => {
                    if let Some(reason) = value.get("stopReason").and_then(|value| value.as_str()) {
                        self.stop_reason = reason.to_string();
                    }
                    if let Some(usage) = value.get("tokenUsage").filter(|value| value.is_object()) {
                        self.usage.reported = true;
                        // `uncachedInputTokens` 已排除缓存；只有它缺失时才用
                        // `inputTokens`（部分实现在那里给总数），两者不能相加。
                        let uncached = number(usage.get("uncachedInputTokens"));
                        self.usage.input += if uncached > 0 {
                            uncached
                        } else {
                            number(usage.get("inputTokens"))
                        };
                        self.usage.output += number(usage.get("outputTokens"));
                        self.usage.cache_read += number(usage.get("cacheReadInputTokens"));
                        self.usage.cache_write += number(usage.get("cacheWriteInputTokens"));
                        let percentage = number_f(
                            value
                                .get("contextUsagePercentage")
                                .unwrap_or(&serde_json::Value::Null),
                        );
                        if percentage > 0.0 {
                            self.percentage = percentage;
                        }
                    }
                }
                "contextUsageEvent" => {
                    let percentage = number_f(
                        value
                            .get("contextUsagePercentage")
                            .unwrap_or(&serde_json::Value::Null),
                    );
                    if percentage > 0.0 {
                        self.percentage = percentage;
                    }
                }
                _ => {}
            }
        }
    }

    fn tool_event(&mut self, value: &serde_json::Value) {
        let id = value
            .get("toolUseId")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let name = value
            .get("name")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        if !id.is_empty() && self.current_tool.as_deref() != Some(id.as_str()) {
            // 同一工具的后续增量不重复发 ToolStart。
            self.current_tool = Some(id.clone());
            self.tools += 1;
            self.pending.push_back(Event::ToolStart { id, name });
        }
        if let Some(input) = value.get("input").filter(|value| !value.is_null()) {
            if let Some(text) = input.as_str() {
                if !text.is_empty() {
                    self.said += text.chars().count();
                    self.pending.push_back(Event::ToolArgs(text.to_string()));
                }
            } else {
                let encoded = serde_json::to_string(input).unwrap_or_default();
                if encoded != "{}" {
                    self.said += encoded.chars().count();
                    self.pending.push_back(Event::ToolArgs(encoded));
                }
            }
        }
    }

    /// 收尾：思考残留、错误、用量与停止原因。
    fn finish(&mut self) {
        self.finished = true;
        if self.split_thinking {
            for event in self.think.end() {
                self.pending.push_back(event);
            }
        }
        if let Some(error) = self.failed.take() {
            let status = if super::errors::stream_error_is_too_long(&error) {
                Some(400)
            } else if super::errors::stream_error_is_quota(&error) {
                Some(429)
            } else {
                None
            };
            self.pending.push_back(Event::Error {
                status,
                text: error,
            });
            return;
        }
        let window = super::models::window_of(&self.account, &self.model);
        if self.usage.input == 0 && self.percentage > 0.0 && window > 0 {
            self.usage.input = ((self.percentage / 100.0) * window as f64) as i64;
        }
        if self.usage.output == 0 {
            self.usage.output = ((self.said + 3) / 4) as i64;
        }
        let full = self
            .stop_reason
            .eq_ignore_ascii_case("model_context_window_exceeded");
        if full && self.said == 0 && self.tools == 0 {
            self.pending.push_back(Event::Error {
                status: Some(400),
                text: format!(
                    "{}{}",
                    super::errors::TOO_LONG,
                    "the reply stopped as the model's context window was full (MODEL_CONTEXT_WINDOW_EXCEEDED)"
                ),
            });
            return;
        }
        let reason = if self.tools > 0 {
            "tool_calls"
        } else if full {
            "length"
        } else if self.stop_reason.eq_ignore_ascii_case("max_tokens") {
            "length"
        } else if self.stop_reason.eq_ignore_ascii_case("content_filtered") {
            "content_filter"
        } else {
            "stop"
        };
        self.pending.push_back(Event::Stop {
            reason: reason.to_string(),
            usage: self.usage,
        });
    }
}

fn number(value: Option<&serde_json::Value>) -> i64 {
    value.and_then(|value| value.as_i64()).unwrap_or(0)
}

fn number_f(value: &serde_json::Value) -> f64 {
    value.as_f64().unwrap_or(0.0)
}

/// `<thinking>` 标签切分器（与参考实现的 `ThinkParser` 同构）。
#[derive(Default)]
pub struct ThinkParser {
    state: u8,
    buffer: String,
    said: bool,
}

const OPEN: &str = "<thinking>";
const CLOSE: &str = "</thinking>";

impl ThinkParser {
    fn feed(&mut self, text: &str) -> Vec<Event> {
        self.buffer.push_str(text);
        let mut out = Vec::new();
        loop {
            match self.state {
                0 => {
                    let trimmed = self.buffer.trim_start_matches([' ', '\t', '\r', '\n']);
                    if trimmed.starts_with(OPEN) {
                        self.buffer = trimmed[OPEN.len()..].to_string();
                        self.state = 1;
                        continue;
                    }
                    if trimmed.len() < OPEN.len() && OPEN.starts_with(trimmed) {
                        return out;
                    }
                    self.state = 2;
                }
                1 => {
                    if !self.said {
                        self.buffer = self.buffer.trim_start_matches(['\r', '\n']).to_string();
                        if self.buffer.is_empty() {
                            return out;
                        }
                    }
                    if let Some(index) = self.buffer.find(CLOSE) {
                        if index > 0 {
                            out.push(Event::Think(self.buffer[..index].to_string()));
                            self.said = true;
                        }
                        self.buffer = self.buffer[index + CLOSE.len()..]
                            .trim_start_matches(['\r', '\n'])
                            .to_string();
                        self.state = 2;
                        continue;
                    }
                    let hold = partial_suffix(&self.buffer, CLOSE);
                    let keep = self.buffer.len().saturating_sub(hold);
                    if keep > 0 {
                        out.push(Event::Think(self.buffer[..keep].to_string()));
                        self.buffer = self.buffer[keep..].to_string();
                        self.said = true;
                    }
                    return out;
                }
                _ => {
                    if !self.buffer.is_empty() {
                        out.push(Event::Text(std::mem::take(&mut self.buffer)));
                    }
                    return out;
                }
            }
        }
    }

    pub fn end(&mut self) -> Vec<Event> {
        if self.buffer.is_empty() {
            return Vec::new();
        }
        let event = if self.state == 1 {
            Event::Think(std::mem::take(&mut self.buffer))
        } else {
            Event::Text(std::mem::take(&mut self.buffer))
        };
        self.state = 2;
        vec![event]
    }
}

/// 结尾有多长的「可能是标签开头」的后缀。
fn partial_suffix(text: &str, tag: &str) -> usize {
    let limit = (tag.len() - 1).min(text.len());
    for length in (1..=limit).rev() {
        if text.ends_with(&tag[..length]) {
            return length;
        }
    }
    0
}
