//! Command Code AI SDK NDJSON → 标准 OpenAI Chat SSE。
//! 对齐CLI的完整工具调用与usage事件；保留最后一步finish-step的兼容收尾。
//! 首个有效内容前不发送HTTP 200，流内失败交回宿主且不伪造完成或零计量。

use super::{errors, fingerprint};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use std::collections::{BTreeMap, BTreeSet};

const MAX_LINE_BYTES: usize = 1024 * 1024;
const MAX_TOOLS: usize = 128;

struct ToolInput {
    name: String,
    text: String,
}

pub struct Output {
    pending: Vec<u8>,
    model: String,
    id: String,
    created: i64,
    pub started: bool,
    pub finished: bool,
    saw_finish: bool,
    step_finish_reason: Option<String>,
    tool_index: i64,
    tools: BTreeMap<String, ToolInput>,
    emitted_tools: BTreeSet<String>,
    server_tools: BTreeSet<String>,
    cache_write: i64,
    usage: Option<Value>,
}

impl Output {
    pub fn new(model: String) -> Result<Self> {
        Ok(Self {
            pending: Vec::new(),
            model,
            id: format!("chatcmpl-{}", fingerprint::random_hex(16)?),
            created: sdk::now() / 1000,
            started: false,
            finished: false,
            saw_finish: false,
            step_finish_reason: None,
            tool_index: 0,
            tools: BTreeMap::new(),
            emitted_tools: BTreeSet::new(),
            server_tools: BTreeSet::new(),
            cache_write: 0,
            usage: None,
        })
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<()> {
        let mut start = 0;
        for (end, byte) in bytes.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            self.append(&bytes[start..end])?;
            let line = std::mem::take(&mut self.pending);
            self.line(&line)?;
            if self.finished {
                return Ok(());
            }
            start = end + 1;
        }
        self.append(&bytes[start..])
    }

    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        if self.pending.len().saturating_add(bytes.len()) > MAX_LINE_BYTES {
            return Err(
                Error::http(502, "Command Code 流式事件超过大小限制").with_code("protocol_error")
            );
        }
        self.pending.extend_from_slice(bytes);
        Ok(())
    }

    fn line(&mut self, bytes: &[u8]) -> Result<()> {
        let line = std::str::from_utf8(bytes)
            .map_err(|_| Error::http(502, "Command Code 流式事件不是有效 UTF-8"))?
            .trim();
        if line.is_empty() || line == "[DONE]" || line.starts_with(':') {
            return Ok(());
        }
        let event: Value = serde_json::from_str(line).map_err(|_| {
            Error::http(502, "Command Code 流式事件不是有效 JSON").with_code("protocol_error")
        })?;
        if !event.is_object() {
            return Err(Error::http(502, "Command Code 流式事件缺少对象结构"));
        }
        match event["type"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "start" | "text-start" | "text-end" | "reasoning-start" | "reasoning-end"
            | "provider-metadata" => {}
            "start-step" => {
                self.saw_finish = false;
                self.step_finish_reason = None;
            }
            "tool-input-start" => {
                let id = event["toolCallId"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| Error::http(502, "Command Code 工具输入缺少调用标识"))?;
                if event["providerExecuted"] == true {
                    if self.server_tools.len() >= MAX_TOOLS {
                        return Err(Error::http(502, "Command Code 服务端工具数量超过限制"));
                    }
                    self.server_tools.insert(id.to_string());
                    return Ok(());
                }
                if self.tools.len() + self.emitted_tools.len() >= MAX_TOOLS
                    || self.tools.contains_key(id)
                    || self.emitted_tools.contains(id)
                {
                    return Err(Error::http(502, "Command Code 工具输入重复或超过数量限制"));
                }
                self.tools.insert(
                    id.to_string(),
                    ToolInput {
                        name: event["toolName"].as_str().unwrap_or("").to_string(),
                        text: String::new(),
                    },
                );
            }
            "tool-input-delta" => {
                let id = event["toolCallId"].as_str().unwrap_or("");
                if event["providerExecuted"] == true || self.server_tools.contains(id) {
                    return Ok(());
                }
                let input = self
                    .tools
                    .get_mut(id)
                    .ok_or_else(|| Error::http(502, "Command Code 工具参数与声明不匹配"))?;
                let delta = event["inputTextDelta"]
                    .as_str()
                    .or_else(|| event["delta"].as_str())
                    .unwrap_or("");
                if input.text.len().saturating_add(delta.len()) > MAX_LINE_BYTES {
                    return Err(Error::http(502, "Command Code 工具参数超过大小限制"));
                }
                input.text.push_str(delta);
            }
            "tool-input-end" => {}
            "tool-error" => return Err(errors::event_error(&event)),
            "cache-write-tokens" => {
                self.cache_write = self
                    .cache_write
                    .max(number(&event, &["cacheWriteTokens"]).max(0));
            }
            "abort" => {
                return Err(
                    Error::http(502, "Command Code 上游已中止回答，未收到完整结果")
                        .with_code("runtime_stream_interrupted"),
                )
            }
            "text-delta" => {
                let text = event["text"]
                    .as_str()
                    .filter(|text| !text.is_empty())
                    .or_else(|| event["delta"].as_str())
                    .unwrap_or("");
                if !text.is_empty() {
                    self.start()?;
                    self.delta(json!({ "content": text }))?;
                }
            }
            "reasoning-delta" => {
                if let Some(text) = event["text"]
                    .as_str()
                    .filter(|text| !text.is_empty())
                    .or_else(|| event["delta"].as_str().filter(|text| !text.is_empty()))
                {
                    self.start()?;
                    self.delta(json!({ "reasoning_content": text }))?;
                }
            }
            "tool-call" => {
                let id = match event["toolCallId"].as_str().filter(|id| !id.is_empty()) {
                    Some(id) => id.to_string(),
                    None => format!("call_{}", fingerprint::random_hex(16)?),
                };
                if event["providerExecuted"] == true || self.server_tools.remove(&id) {
                    self.tools.remove(&id);
                    return Ok(());
                }
                if self.emitted_tools.contains(&id) || self.emitted_tools.len() >= MAX_TOOLS {
                    return Err(Error::http(502, "Command Code 工具调用重复或超过数量限制"));
                }
                let pending = self.tools.remove(&id);
                let name = event["toolName"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .or_else(|| {
                        pending
                            .as_ref()
                            .map(|input| input.name.as_str())
                            .filter(|name| !name.is_empty())
                    })
                    .ok_or_else(|| Error::http(502, "Command Code 工具调用缺少名称"))?;
                let input = event
                    .get("input")
                    .filter(|value| !value.is_null())
                    .or_else(|| event.get("args").filter(|value| !value.is_null()));
                let arguments = match input {
                    Some(Value::String(text)) => {
                        if !serde_json::from_str::<Value>(text).is_ok_and(|value| value.is_object())
                        {
                            return Err(Error::http(
                                502,
                                "Command Code 工具参数不是完整 JSON 对象",
                            ));
                        }
                        text.clone()
                    }
                    Some(value) if value.is_object() => value.to_string(),
                    Some(_) => return Err(Error::http(502, "Command Code 工具参数形态无效")),
                    None => match pending
                        .as_ref()
                        .map(|input| input.text.as_str())
                        .filter(|text| !text.is_empty())
                    {
                        Some(text)
                            if serde_json::from_str::<Value>(text)
                                .is_ok_and(|value| value.is_object()) =>
                        {
                            text.to_string()
                        }
                        Some(_) => {
                            return Err(Error::http(502, "Command Code 工具参数未完成，未补空对象"))
                        }
                        None => "{}".to_string(),
                    },
                };
                let index = self.tool_index;
                self.tool_index += 1;
                self.start()?;
                self.delta(
                    json!({ "tool_calls": [{ "index": index, "id": id, "type": "function",
                    "function": { "name": name, "arguments": arguments } }] }),
                )?;
                self.emitted_tools.insert(id);
            }
            "finish-step" => {
                self.saw_finish = true;
                if let Some(reason) = event["finishReason"]
                    .as_str()
                    .map(str::trim)
                    .filter(|reason| !reason.is_empty())
                {
                    self.step_finish_reason = Some(map_finish_reason(reason));
                }
                if event["usage"].is_object() {
                    self.usage = Some(normalized_usage(&event["usage"]));
                }
            }
            "finish" => {
                self.saw_finish = true;
                let reason = event["finishReason"]
                    .as_str()
                    .map(str::trim)
                    .filter(|reason| !reason.is_empty())
                    .map(map_finish_reason)
                    .or_else(|| self.step_finish_reason.clone())
                    .unwrap_or_else(|| "stop".to_string());
                if let Some(usage) = event
                    .get("totalUsage")
                    .filter(|usage| usage.is_object())
                    .or_else(|| event.get("usage").filter(|usage| usage.is_object()))
                {
                    self.usage = Some(normalized_usage(usage));
                }
                self.complete(&reason)?;
            }
            "error" => return Err(errors::event_error(&event)),
            // 官方新增的旁路事件不能变成用户正文；没有内容/结束信号仍会在 EOF 报错。
            _ => {}
        }
        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        if !self.started {
            sdk::stream_head(200)?;
            self.started = true;
            self.delta(json!({ "role": "assistant", "content": "" }))?;
        }
        Ok(())
    }

    fn frame(&self, choices: Value) -> Value {
        json!({ "id": self.id, "object": "chat.completion.chunk", "created": self.created,
            "model": self.model, "choices": choices })
    }

    fn delta(&self, delta: Value) -> Result<()> {
        sdk::stream_event(
            &self.frame(json!([{ "index": 0, "delta": delta, "finish_reason": null }])),
        )
    }

    fn complete(&mut self, reason: &str) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        if reason == "upstream_error" {
            return Err(
                Error::http(502, "Command Code 上游报告连接失败").with_code("upstream_error")
            );
        }
        if !self.tools.is_empty() {
            return Err(Error::http(502, "Command Code 工具输入在完成前被截断")
                .with_code("runtime_stream_interrupted"));
        }
        if !self.started && reason != "content_filter" {
            return Err(
                Error::http(502, "Command Code 上游返回空响应（没有任何内容事件）")
                    .with_code("protocol_error"),
            );
        }
        self.start()?;
        let reason = match reason {
            "pause_turn" => "length",
            "stop" if self.tool_index > 0 => "tool_calls",
            "stop" | "length" | "tool_calls" | "content_filter" | "function_call" => reason,
            _ => "stop",
        };
        sdk::stream_event(
            &self.frame(json!([{ "index": 0, "delta": {}, "finish_reason": reason }])),
        )?;
        if let Some(reading) = self.usage.as_ref() {
            let mut usage = self.frame(json!([]));
            let mut reading = reading.clone();
            if self.cache_write > 0 {
                let previous = reading["prompt_tokens_details"]["cache_write_tokens"]
                    .as_i64()
                    .unwrap_or(0);
                reading["prompt_tokens_details"]["cache_write_tokens"] =
                    json!(previous.max(self.cache_write));
            }
            usage["usage"] = reading;
            sdk::stream_event(&usage)?;
        }
        sdk::stream_bytes(b"data: [DONE]\n\n")?;
        self.finished = true;
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.line(&line)?;
            if self.finished {
                return Ok(());
            }
        }
        if !self.saw_finish {
            return Err(
                Error::http(502, "Command Code 上游流提前中断，未收到完成事件")
                    .with_code("runtime_stream_interrupted"),
            );
        }
        let reason = self
            .step_finish_reason
            .clone()
            .unwrap_or_else(|| "stop".into());
        self.complete(&reason)
    }
}

fn map_finish_reason(reason: &str) -> String {
    let value = reason.trim().to_ascii_lowercase();
    match value.as_str() {
        "" => "stop".into(),
        "tool-calls" | "tool_calls" | "tool_use" => "tool_calls".into(),
        "content-filter" | "content_filter" => "content_filter".into(),
        "length" | "max_tokens" | "max_output_tokens" | "model_context_window_exceeded" => {
            "length".into()
        }
        "error" => "upstream_error".into(),
        other
            if ["network", "connection", "upstream"].iter().any(|prefix| {
                other
                    .strip_prefix(*prefix)
                    .is_some_and(|rest| rest.trim_start_matches(['-', '_', ' ']) == "error")
            }) =>
        {
            "upstream_error".into()
        }
        _ => value,
    }
}

fn number(value: &Value, keys: &[&str]) -> i64 {
    keys.iter()
        .find_map(|key| {
            value[*key].as_i64().or_else(|| {
                value[*key]
                    .as_f64()
                    .filter(|value| value.is_finite())
                    .map(|value| value as i64)
            })
        })
        .unwrap_or(0)
}

fn normalized_usage(value: &Value) -> Value {
    // AI SDK 5/6均可能使用嵌套tokenDetails；零输出仍保留实际输入和缓存读写，不能伪造零计量。
    let output = number(
        value,
        &["outputTokens", "completionTokens", "completion_tokens"],
    )
    .max(0);
    let input = number(value, &["inputTokens", "promptTokens", "prompt_tokens"]).max(0);
    let cached = number(&value["inputTokenDetails"], &["cacheReadTokens"])
        .max(number(value, &["cachedInputTokens", "cacheReadTokens"]))
        .max(number(&value["prompt_tokens_details"], &["cached_tokens"]))
        .clamp(0, input);
    let written = number(&value["inputTokenDetails"], &["cacheWriteTokens"])
        .max(number(
            value,
            &["cacheWriteTokens", "cacheCreationInputTokens"],
        ))
        .max(number(
            &value["prompt_tokens_details"],
            &["cache_write_tokens"],
        ))
        .max(0);
    let reasoning = number(&value["outputTokenDetails"], &["reasoningTokens"])
        .max(number(
            &value["completion_tokens_details"],
            &["reasoning_tokens"],
        ))
        .clamp(0, output);
    json!({ "prompt_tokens": input, "completion_tokens": output, "total_tokens": input.saturating_add(output),
        "prompt_tokens_details": { "cached_tokens": cached, "cache_write_tokens": written },
        "completion_tokens_details": { "reasoning_tokens": reasoning } })
}
