//! Trae output/token_usage/done → 标准Chat SSE。usage仅投影已给字段，不估算零值。
//! 首有效输出前不发head；开始后错误不重放。error后只收真实usage，done/EOF均不改成功。

use super::{errors, events::Event, util};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub struct Output {
    id: String,
    model: String,
    created: i64,
    pub started: bool,
    pub terminal: bool,
    pub failure: Option<Error>,
    usage: Option<Value>,
    reasoning: String,
    last_reasoning: String,
    tools_seen: bool,
    tool_calls: Vec<ToolCall>,
    ended: bool,
}

#[derive(Default)]
struct ToolCall {
    id: String,
    upstream_index: Option<u64>,
    name: String,
    arguments: String,
}

const MAX_TOOL_BUFFER: usize = 4 * 1024 * 1024;

fn usage(value: &Value) -> Option<Value> {
    let value = value
        .get("usage")
        .filter(|v| v.is_object())
        .unwrap_or(value);
    let mut out = json!({});
    for key in [
        "prompt_tokens",
        "completion_tokens",
        "total_tokens",
        "input_tokens",
        "output_tokens",
        "promptTokens",
        "completionTokens",
        "totalTokens",
        "inputTokens",
        "outputTokens",
        "reasoning_tokens",
        "cached_tokens",
        "cache_read_input_tokens",
        "cacheReadInputTokens",
        "cache_creation_input_tokens",
        "cacheCreationInputTokens",
        "prompt_cache_hit_tokens",
    ] {
        if let Some(v) = util::integer(&value[key]).filter(|v| *v >= 0) {
            out[key] = json!(v);
        }
    }
    for key in [
        "prompt_tokens_details",
        "completion_tokens_details",
        "input_tokens_details",
        "output_tokens_details",
    ] {
        let mut details = json!({});
        for field in [
            "cached_tokens",
            "cache_write_tokens",
            "reasoning_tokens",
            "audio_tokens",
            "accepted_prediction_tokens",
            "rejected_prediction_tokens",
        ] {
            if let Some(v) = value
                .get(key)
                .and_then(|d| d.get(field))
                .and_then(util::integer)
                .filter(|v| *v >= 0)
            {
                details[field] = json!(v);
            }
        }
        if details.as_object().is_some_and(|d| !d.is_empty()) {
            out[key] = details;
        }
    }
    for (target, aliases) in [
        (
            "prompt_tokens",
            &[
                "prompt_tokens",
                "input_tokens",
                "promptTokens",
                "inputTokens",
            ][..],
        ),
        (
            "completion_tokens",
            &[
                "completion_tokens",
                "output_tokens",
                "completionTokens",
                "outputTokens",
            ][..],
        ),
        ("total_tokens", &["total_tokens", "totalTokens"][..]),
        (
            "cache_read_input_tokens",
            &["cache_read_input_tokens", "cacheReadInputTokens"][..],
        ),
        (
            "cache_creation_input_tokens",
            &["cache_creation_input_tokens", "cacheCreationInputTokens"][..],
        ),
    ] {
        let mut zero = None;
        let count = aliases
            .iter()
            .filter_map(|key| util::integer(&value[*key]))
            .filter(|n| *n >= 0)
            .find(|n| {
                if *n == 0 {
                    zero = Some(0);
                    false
                } else {
                    true
                }
            })
            .or(zero);
        if let Some(count) = count {
            out[target] = json!(count);
        }
    }
    if let Some(thought) = util::integer(&value["reasoning_tokens"]).filter(|n| *n >= 0) {
        if !out["completion_tokens_details"].is_object() {
            out["completion_tokens_details"] = json!({});
        }
        if out["completion_tokens_details"]["reasoning_tokens"].is_null() {
            out["completion_tokens_details"]["reasoning_tokens"] = json!(thought);
        }
    }
    let read = [
        out.pointer("/prompt_tokens_details/cached_tokens"),
        out.pointer("/input_tokens_details/cached_tokens"),
        out.get("cached_tokens"),
        out.get("prompt_cache_hit_tokens"),
    ]
    .into_iter()
    .flatten()
    .filter_map(util::integer)
    .filter(|n| *n >= 0)
    .max();
    if let Some(read) = read {
        if !out["prompt_tokens_details"].is_object() {
            out["prompt_tokens_details"] = json!({});
        }
        out["prompt_tokens_details"]["cached_tokens"] = json!(read);
    }
    if let Some((prompt, completion)) = out["prompt_tokens"]
        .as_i64()
        .zip(out["completion_tokens"].as_i64())
    {
        out["total_tokens"] = json!(out["total_tokens"]
            .as_i64()
            .unwrap_or(0)
            .max(prompt.saturating_add(completion)));
    }
    let known = [
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
        "cached_tokens",
        "prompt_cache_hit_tokens",
    ]
    .iter()
    .any(|key| out.get(*key).is_some())
        || out
            .pointer("/prompt_tokens_details/cached_tokens")
            .is_some()
        || out.pointer("/input_tokens_details/cached_tokens").is_some()
        || out
            .pointer("/prompt_tokens_details/cache_write_tokens")
            .is_some();
    if !known && out.as_object().is_some_and(|fields| !fields.is_empty()) {
        out["magpie_cache_unknown"] = json!(true);
    }
    out.as_object()
        .filter(|v| !v.is_empty())
        .map(|_| out.clone())
}

fn merge_usage(previous: Option<Value>, incoming: Value) -> Value {
    let mut previous = previous.unwrap_or_else(|| json!({}));
    if incoming.get("prompt_tokens").is_some() || incoming.get("completion_tokens").is_some() {
        previous
            .as_object_mut()
            .map(|fields| fields.remove("total_tokens"));
    }
    let names_cache = incoming.get("cache_read_input_tokens").is_some()
        || incoming.get("cache_creation_input_tokens").is_some()
        || incoming.get("cached_tokens").is_some()
        || incoming.get("prompt_cache_hit_tokens").is_some()
        || incoming
            .pointer("/prompt_tokens_details/cached_tokens")
            .is_some()
        || incoming
            .pointer("/input_tokens_details/cached_tokens")
            .is_some();
    if names_cache {
        previous
            .as_object_mut()
            .map(|fields| fields.remove("magpie_cache_unknown"));
    }
    for (key, value) in incoming.as_object().into_iter().flatten() {
        if value.is_object() {
            if !previous[key].is_object() {
                previous[key] = json!({});
            }
            for (field, value) in value.as_object().into_iter().flatten() {
                previous[key][field] = value.clone();
            }
        } else {
            previous[key] = value.clone();
        }
    }
    if let Some((prompt, completion)) = previous["prompt_tokens"]
        .as_i64()
        .zip(previous["completion_tokens"].as_i64())
    {
        previous["total_tokens"] = json!(previous["total_tokens"]
            .as_i64()
            .unwrap_or(0)
            .max(prompt.saturating_add(completion)));
    }
    // 末帧明确上报的零覆盖旧读数；没有上报的项仍保留，不能估算成零。
    previous
}

fn complete_arguments(value: &str) -> bool {
    !value.is_empty() && serde_json::from_str::<Value>(value).is_ok()
}

fn merge_piece(current: &mut String, incoming: &str) -> Result<()> {
    if incoming.is_empty() || current == incoming || current.starts_with(incoming) {
        return Ok(());
    }
    if incoming.starts_with(current.as_str()) {
        *current = incoming.into();
    } else if complete_arguments(incoming) && !complete_arguments(&(current.clone() + incoming)) {
        *current = incoming.into();
    } else {
        if current.len().saturating_add(incoming.len()) > MAX_TOOL_BUFFER {
            return Err(Error::http(502, "Trae 工具参数超过受控缓冲大小"));
        }
        current.push_str(incoming);
    }
    Ok(())
}

impl Output {
    pub fn new(model: &str) -> Result<Self> {
        Ok(Self {
            id: format!("chatcmpl-{}", util::random_hex(16)?),
            model: model.into(),
            created: util::now()? / 1000,
            started: false,
            terminal: false,
            failure: None,
            usage: None,
            reasoning: String::new(),
            last_reasoning: String::new(),
            tools_seen: false,
            tool_calls: Vec::new(),
            ended: false,
        })
    }

    fn head(&mut self) -> Result<()> {
        if !self.started {
            sdk::stream_head(200)?;
            self.started = true;
        }
        Ok(())
    }

    fn frame(&mut self, delta: Value, finish: Option<&str>, only_usage: bool) -> Result<()> {
        let choices = if only_usage {
            json!([])
        } else {
            json!([{ "index": 0, "delta": delta, "finish_reason": finish }])
        };
        let mut frame = json!({ "id": self.id, "object": "chat.completion.chunk", "created": self.created,
            "model": self.model, "choices": choices });
        if let Some(usage) = self.usage.take() {
            frame["usage"] = usage;
        }
        self.head()?;
        sdk::stream_event(&frame)
    }

    fn reasoning(&mut self, incoming: String) -> Result<String> {
        if self.reasoning.len().saturating_add(incoming.len()) > 8 * 1024 * 1024 {
            return Err(Error::http(502, "Trae 思考流超过受控缓冲大小"));
        }
        let duplicate = incoming == self.last_reasoning && incoming.chars().count() >= 8
            || self.reasoning.chars().count() >= 16
                && incoming.chars().count() >= 16
                && self.reasoning.ends_with(&incoming);
        self.last_reasoning = incoming.clone();
        if duplicate {
            return Ok(String::new());
        }
        let delta = if self.reasoning.chars().count() >= 16 {
            incoming
                .strip_prefix(&self.reasoning)
                .unwrap_or(&incoming)
                .to_string()
        } else {
            incoming
        };
        self.reasoning.push_str(&delta);
        Ok(delta)
    }

    pub fn fail(&mut self, error: Error) -> Result<()> {
        if self.failure.is_none() {
            if self.started {
                sdk::stream_event(&errors::stream(&error))?;
            }
            self.failure = Some(error);
        }
        Ok(())
    }

    fn observe_tools(&mut self, value: Value) -> Result<()> {
        let calls = match value {
            Value::Array(calls) => calls,
            Value::Object(_) => vec![value],
            _ => return Err(Error::http(502, "Trae 工具调用片段格式无效")),
        };
        if calls.len() > 512 {
            return Err(Error::http(502, "Trae 工具数量超过限制"));
        }
        for call in calls {
            if !call.is_object() {
                return Err(Error::http(502, "Trae 工具调用片段不是对象"));
            }
            let function = call
                .get("function_call")
                .or_else(|| call.get("function"))
                .unwrap_or(&call);
            if !function.is_object() {
                return Err(Error::http(502, "Trae 工具参数格式无效"));
            }
            let id = util::text(&call, &["id", "tool_call_id"]);
            let name = util::text(function, &["name"]);
            if id.len() > 1024
                || name.len() > 512
                || id.chars().chain(name.chars()).any(char::is_control)
            {
                return Err(Error::http(502, "Trae 工具名称或标识无效"));
            }
            let upstream_index = match call.get("index") {
                None | Some(Value::Null) => None,
                Some(value) => Some(
                    value
                        .as_u64()
                        .filter(|index| *index <= 4096)
                        .ok_or_else(|| Error::http(502, "Trae 工具索引无效"))?,
                ),
            };
            let arguments = function
                .get("arguments")
                .or_else(|| function.get("args"))
                .or_else(|| call.get("params"))
                .or_else(|| call.get("input"))
                .or_else(|| call.get("parameters"));
            let arguments = match arguments {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(text)) => text.clone(),
                Some(value) => value.to_string(),
            };
            if arguments.len() > MAX_TOOL_BUFFER {
                return Err(Error::http(502, "Trae 工具参数过长"));
            }
            let by_id = (!id.is_empty())
                .then(|| self.tool_calls.iter().position(|old| old.id == id))
                .flatten();
            let by_index = upstream_index.and_then(|index| {
                self.tool_calls
                    .iter()
                    .position(|old| old.upstream_index == Some(index))
            });
            if by_id.zip(by_index).is_some_and(|(id, index)| id != index) {
                return Err(Error::http(502, "Trae 工具片段标识冲突"));
            }
            let index = by_id.or(by_index).or_else(|| {
                if !id.is_empty() || upstream_index.is_some() {
                    return None;
                }
                self.tool_calls.iter().rposition(|old| {
                    (name.is_empty() || old.name == name) && !complete_arguments(&old.arguments)
                })
            });
            let index = match index {
                Some(index) => index,
                None => {
                    if name.is_empty() && id.is_empty() && upstream_index.is_none() {
                        return Err(Error::http(502, "Trae 工具片段没有可确定的归属"));
                    }
                    if self.tool_calls.len() >= 512 {
                        return Err(Error::http(502, "Trae 工具数量超过限制"));
                    }
                    self.tool_calls.push(ToolCall::default());
                    self.tool_calls.len() - 1
                }
            };
            let target = &mut self.tool_calls[index];
            if !id.is_empty() {
                if !target.id.is_empty() && target.id != id {
                    return Err(Error::http(502, "Trae 工具ID已变化"));
                }
                target.id = id;
            }
            if let Some(index) = upstream_index {
                if target.upstream_index.is_some_and(|old| old != index) {
                    return Err(Error::http(502, "Trae 工具索引已变化"));
                }
                target.upstream_index = Some(index);
            }
            if !name.is_empty() {
                if target.name.is_empty() || name.starts_with(target.name.as_str()) {
                    target.name = name;
                } else if !target.name.starts_with(&name) {
                    target.name.push_str(&name);
                }
            }
            merge_piece(&mut target.arguments, &arguments)?;
            if self
                .tool_calls
                .iter()
                .map(|call| call.arguments.len() + call.name.len() + call.id.len())
                .sum::<usize>()
                > MAX_TOOL_BUFFER
            {
                return Err(Error::http(502, "Trae 全部工具参数超过受控缓冲大小"));
            }
        }
        Ok(())
    }

    fn flush_tools(&mut self) -> Result<()> {
        let mut calls = Vec::new();
        for (index, call) in self.tool_calls.iter_mut().enumerate() {
            if call.name.is_empty() {
                return Err(Error::http(502, "Trae 工具调用缺少名称"));
            }
            if call.arguments.is_empty() {
                call.arguments = "{}".into();
            }
            if !complete_arguments(&call.arguments) {
                return Err(Error::http(
                    502,
                    "Trae 工具参数在完成前被截断，未提交不完整调用",
                ));
            }
            if call.id.is_empty() {
                call.id = format!("call_{}", util::random_hex(12)?);
            }
            calls.push(json!({ "index": index, "id": call.id, "type": "function",
                "function": { "name": call.name, "arguments": call.arguments } }));
        }
        if !calls.is_empty() {
            self.tools_seen = true;
            self.frame(json!({ "tool_calls": calls }), None, false)?;
        }
        Ok(())
    }

    pub fn apply(&mut self, event: Event) -> Result<()> {
        if self.terminal {
            return Ok(());
        }
        match event {
            Event::Error(error) => self.fail(error)?,
            Event::Other => {}
            Event::Usage(value) => {
                if let Some(value) = usage(&value) {
                    self.usage = Some(merge_usage(self.usage.take(), value));
                }
            }
            Event::Output {
                text,
                reasoning,
                tools,
            } if self.failure.is_none() => {
                let mut delta = json!({});
                if !text.is_empty() {
                    delta["content"] = json!(text);
                }
                let reasoning = self.reasoning(reasoning)?;
                if !reasoning.is_empty() {
                    delta["reasoning_content"] = json!(reasoning);
                }
                if let Some(tools) = tools {
                    self.observe_tools(tools)?;
                }
                if delta.as_object().is_some_and(|v| !v.is_empty()) {
                    self.frame(delta, None, false)?;
                }
            }
            Event::Output { .. } => {}
            Event::Done {
                reason,
                usage: final_usage,
            } => {
                if let Some(value) = final_usage.as_ref().and_then(usage) {
                    self.usage = Some(merge_usage(self.usage.take(), value));
                }
                self.terminal = true;
                if matches!(reason.as_str(), "content_filter" | "safety") {
                    self.fail(errors::upstream(
                        403,
                        &json!({ "providerCode": "trae_content_blocked" }),
                    ))?;
                } else if !matches!(
                    reason.as_str(),
                    "stop" | "tool_calls" | "function_call" | "length"
                ) {
                    self.fail(Error::http(502, "Trae 未给出可识别的结束原因"))?;
                }
                if self.failure.is_none() {
                    if let Err(error) = self.flush_tools() {
                        self.fail(error)?;
                    }
                }
                if self.failure.is_none() && !self.started {
                    self.fail(Error::http(502, "Trae 结束前没有返回有效回答"))?;
                }
                if self.failure.is_none() {
                    let reason = if reason == "stop" && self.tools_seen {
                        "tool_calls"
                    } else {
                        &reason
                    };
                    self.frame(json!({}), Some(reason), false)?;
                } else if self.started && self.usage.is_some() {
                    self.frame(json!({}), None, true)?;
                }
                if self.started {
                    sdk::stream_bytes(b"data: [DONE]\n\n")?;
                }
            }
        }
        Ok(())
    }

    pub fn observed_tools(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    pub fn end(&mut self) -> Result<()> {
        if self.ended {
            return self.failure.clone().map_or(Ok(()), Err);
        }
        self.ended = true;
        if !self.terminal {
            self.fail(
                Error::http(502, "Trae 上游未给出完成事件即中断，回答可能不完整")
                    .with_code("trae_missing_terminal"),
            )?;
            if self.started {
                if self.usage.is_some() {
                    self.frame(json!({}), None, true)?;
                }
                sdk::stream_bytes(b"data: [DONE]\n\n")?;
            }
        }
        match self.failure.clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}
