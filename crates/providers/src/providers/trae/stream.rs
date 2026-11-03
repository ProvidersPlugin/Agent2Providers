//! 上游 SOLO SSE → OpenAI 形状的转换，以及非流式聚合。
//!
//! ── 这条流上有三件事必须先想清楚再写代码 ──────────────────
//! 1. **上游不发 `[DONE]`**。结束帧是 `event: done`，`[DONE]` 是这里补的；
//!    上游中途断掉（没有 done）也要补，否则客户端永远等在最后一片上。
//! 2. **业务错误藏在 200 的流里**（`event: error`）。它不是 chunk，
//!    而是一条 `event: error` + `[DONE]`；分类见 `errors.rs`。
//! 3. `token_usage` 随下一帧发送；EOF 或传输失败时仍须冲刷已收到的用量。
//!
//! 帧的**键顺序**不是契约（Go 的 map 序列化按 key 排序，serde 按插入顺序），
//! 所以测试比的是解析后的 JSON；`id` 与 `created` 每次运行都不同，
//! 向量里已归一成 `<ID>` / `<CREATED>`。

use serde_json::{json, Value};

use super::errors::{error_event_fields, stream_error_kind, ErrorKind};

/// 一条已解析的上游事件。`metadata` / `timing_cost` / `extra_info` / 未知名
/// 都归到 `Other`：当前实现**不处理**它们（`extra_info` 在两个 switch 里都落空，
/// 这是参考实现的既有行为，向量 `extra_info 帧（当前实现不处理）` 钉着）。
#[derive(Clone, Debug, PartialEq)]
pub enum SoloEvent {
    Output {
        response: String,
        reasoning: String,
        tool_calls: Option<Value>,
    },
    TokenUsage(Value),
    Done {
        finish_reason: String,
    },
    Error {
        code: i64,
        message: String,
    },
    Other,
}

/// 解析一条完整事件（`data` 是同一事件内多行 `data:` 累加后的结果）。
/// JSON 不合法时返回 `None` —— 参考实现是"忽略这一帧"，不是"报错断流"。
pub fn parse_solo_event(event: &str, data: &str) -> Option<SoloEvent> {
    if data.trim().is_empty() || data.trim() == "[DONE]" {
        return Some(match event {
            "done" | "response_done" | "stream_done" => SoloEvent::Done {
                finish_reason: "stop".to_string(),
            },
            _ if data.trim() == "[DONE]" => SoloEvent::Done {
                finish_reason: "stop".to_string(),
            },
            _ => SoloEvent::Other,
        });
    }
    let mut parsed: Value = serde_json::from_str(data).ok()?;
    if let Some(inner) = parsed
        .as_str()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
    {
        if inner.is_object() {
            parsed = inner;
        }
    }
    let event = if event.is_empty() {
        parsed
            .get("event")
            .or_else(|| parsed.get("type"))
            .and_then(Value::as_str)
            .unwrap_or_default()
    } else {
        event
    };
    let (code, message) = error_event_fields(&parsed);
    if event == "error"
        || parsed.get("error").is_some_and(|error| !error.is_null())
        || (code != 0
            && !message.is_empty()
            && !parsed.get("response").is_some_and(Value::is_string))
    {
        return Some(SoloEvent::Error { code, message });
    }
    Some(match event {
        "" | "output" | "message" | "delta" | "content" | "text" | "answer" | "chunk"
        | "output_delta" | "message_delta" => {
            let empty = Value::Null;
            let delta = parsed
                .get("delta")
                .filter(|value| value.is_object())
                .unwrap_or(&empty);
            let response = first_text(&[
                parsed.get("response"),
                parsed.get("content"),
                delta.get("content"),
                parsed.get("delta"),
                parsed.get("text"),
                parsed.get("message"),
            ]);
            let reasoning = first_text(&[
                parsed.get("reasoning_content"),
                parsed.get("reasoning"),
                delta.get("reasoning_content"),
                delta.get("reasoning"),
            ]);
            SoloEvent::Output {
                response,
                reasoning,
                tool_calls: parsed
                    .get("tool_calls")
                    .or_else(|| delta.get("tool_calls"))
                    .cloned()
                    .filter(|value| !value.is_null()),
            }
        }
        "token_usage" => SoloEvent::TokenUsage(parsed.get("usage").cloned().unwrap_or(parsed)),
        "done" | "response_done" | "stream_done" => SoloEvent::Done {
            finish_reason: finish_reason(&parsed),
        },
        _ => SoloEvent::Other,
    })
}

fn first_text(values: &[Option<&Value>]) -> String {
    values
        .iter()
        .filter_map(|value| value.and_then(Value::as_str))
        .find(|text| !text.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn finish_reason(value: &Value) -> String {
    let reason = value
        .get("finish_reason")
        .or_else(|| value.get("stop_reason"))
        .or_else(|| value.get("finishReason"))
        .and_then(Value::as_str)
        .unwrap_or("stop");
    match reason {
        "max_tokens" | "max_output_tokens" | "max_length" => "length",
        "sensitive" => "content_filter",
        "" => "stop",
        other => other,
    }
    .to_string()
}

/// 一行一行的 SSE 状态机（跨行累积 `event:` 与多个 `data:`）。
#[derive(Default, Debug)]
pub struct SseScanner {
    event: String,
    data: String,
}

impl SseScanner {
    /// 喂一行（不含行尾换行）。返回该行**触发**的事件；一行可以触发一个，
    /// 也可以像大多数行那样返回 `None`（累积中）。
    pub fn feed_line(&mut self, line: &str) -> Option<SoloEvent> {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            if self.event.is_empty() && self.data.is_empty() {
                self.reset();
                return None;
            }
            let event = parse_solo_event(&self.event, &self.data);
            self.reset();
            return event;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            self.event = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            // 刻意不 trim：参考实现是**直接拼接**，多行 data 之间因此留下
            // 一个空格（`{"response":` + ` "y"}`），拼出来才是合法 JSON。
            self.data.push_str(rest);
        }
        // `:` 开头是注释行，忽略；其它前缀（含 `id:`）同样忽略。
        None
    }

    fn finish(&mut self) -> Option<SoloEvent> {
        let event = parse_solo_event(&self.event, &self.data);
        self.reset();
        event
    }

    fn reset(&mut self) {
        self.event.clear();
        self.data.clear();
    }
}

/// 转换后的一帧出站内容。
#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    /// 一个 OpenAI chunk（`data: {…}`）。
    Chunk(Value),
    /// 流内业务错误：`event: error` + 一句转义后的文案。
    Error {
        code: i64,
        message: String,
        kind: ErrorKind,
    },
    /// `data: [DONE]`
    Done,
}

impl Frame {
    /// 落到线上的字节（不含结尾的两个换行，由调用方补分隔）。
    ///
    /// `#[allow(dead_code)]`：生产路径走 `forward::render`（它要额外把 `model`
    /// 改写成客户端请求的名字），这里是**参考实现的逐字形态**，对拍与排障时
    /// 拿它做基准。
    #[allow(dead_code)]
    pub fn wire(&self) -> String {
        match self {
            Self::Chunk(chunk) => format!("data: {chunk}"),
            Self::Error { code, message, .. } => {
                // 与参考实现同一条文案：`solo error code=<码> msg=<文案>`，
                // 载荷是**一个 JSON 字符串**而不是对象。
                format!(
                    "event: error\ndata: {}",
                    json!(format!("solo error code={code} msg={message}"))
                )
            }
            Self::Done => "data: [DONE]".to_string(),
        }
    }
}

/// 流式转换器。
pub struct SoloStream {
    id: String,
    created: i64,
    scanner: SseScanner,
    /// `token_usage` 读到后欠着，随下一帧一起发。
    pending_usage: Option<Value>,
    reasoning: String,
    last_reasoning: String,
    had_error: bool,
    tool_calls_seen: bool,
    saw_done: bool,
    completed: bool,
}

impl SoloStream {
    pub fn new(id: String, created: i64) -> Self {
        Self {
            id,
            created,
            scanner: SseScanner::default(),
            pending_usage: None,
            reasoning: String::new(),
            last_reasoning: String::new(),
            had_error: false,
            tool_calls_seen: false,
            saw_done: false,
            completed: false,
        }
    }

    /// 喂一行，返回该行产生的帧。
    pub fn feed_line(&mut self, line: &str) -> Vec<Frame> {
        if self.completed {
            return Vec::new();
        }
        let Some(event) = self.scanner.feed_line(line) else {
            return Vec::new();
        };
        self.apply(event)
    }

    fn apply(&mut self, event: SoloEvent) -> Vec<Frame> {
        match event {
            SoloEvent::Output {
                response,
                reasoning,
                tool_calls,
            } => {
                if self.had_error {
                    return Vec::new();
                }
                let mut delta = serde_json::Map::new();
                if !response.is_empty() {
                    delta.insert("content".to_string(), json!(response));
                }
                let reasoning = self.reasoning_delta(reasoning);
                if !reasoning.is_empty() {
                    delta.insert("reasoning_content".to_string(), json!(reasoning));
                }
                if let Some(calls) = tool_calls {
                    let calls = normalize_stream_tool_calls(calls);
                    if calls.as_array().is_some_and(|calls| !calls.is_empty()) {
                        self.tool_calls_seen = true;
                        delta.insert("tool_calls".to_string(), calls);
                    }
                }
                if delta.is_empty() {
                    return Vec::new();
                }
                vec![self.chunk(Value::Object(delta), None)]
            }
            SoloEvent::TokenUsage(usage) => {
                self.pending_usage = Some(usage);
                Vec::new()
            }
            SoloEvent::Done { finish_reason } => {
                self.completed = true;
                let sent = self.saw_done;
                self.saw_done = true;
                let finish = if finish_reason == "stop" && self.tool_calls_seen {
                    "tool_calls".to_string()
                } else {
                    finish_reason
                };
                let mut frames = if self.had_error {
                    if self.pending_usage.is_some() {
                        vec![self.chunk(json!({}), None)]
                    } else {
                        Vec::new()
                    }
                } else {
                    vec![self.chunk(json!({}), Some(finish))]
                };
                if !sent {
                    frames.push(Frame::Done);
                }
                frames
            }
            SoloEvent::Error { code, message } => {
                self.had_error = true;
                let kind = stream_error_kind(code, &message);
                // error 后仍读取可能到来的用量；只有完整 done 事件结束读取。
                vec![Frame::Error {
                    code,
                    message,
                    kind,
                }]
            }
            SoloEvent::Other => Vec::new(),
        }
    }

    fn reasoning_delta(&mut self, incoming: String) -> String {
        let duplicate = (incoming == self.last_reasoning && incoming.chars().count() >= 8)
            || (self.reasoning.chars().count() >= 16
                && incoming.chars().count() >= 16
                && self.reasoning.ends_with(&incoming));
        self.last_reasoning = incoming.clone();
        if duplicate {
            return String::new();
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
        delta
    }

    pub fn completed(&self) -> bool {
        self.completed
    }

    pub fn flush_pending(&mut self) -> Vec<Frame> {
        let mut frames = match self.scanner.finish() {
            Some(event) => self.apply(event),
            None => Vec::new(),
        };
        if self.pending_usage.is_some() {
            frames.push(self.chunk(json!({}), None));
        }
        frames
    }

    /// EOF 时保留已收到的尾数据与用量，结束标记只发一次。
    pub fn finish(&mut self) -> Vec<Frame> {
        let mut frames = self.flush_pending();
        if !self.saw_done {
            frames.push(Frame::Done);
        }
        self.saw_done = true;
        frames
    }

    fn chunk(&mut self, delta: Value, finish_reason: Option<String>) -> Frame {
        let mut choice = serde_json::Map::new();
        choice.insert("index".to_string(), json!(0));
        choice.insert("delta".to_string(), delta);
        if let Some(reason) = finish_reason {
            choice.insert("finish_reason".to_string(), json!(reason));
        }
        let mut chunk = serde_json::Map::new();
        chunk.insert("id".to_string(), json!(self.id));
        chunk.insert("object".to_string(), json!("chat.completion.chunk"));
        chunk.insert("created".to_string(), json!(self.created));
        chunk.insert("model".to_string(), json!(""));
        chunk.insert(
            "choices".to_string(),
            Value::Array([Value::Object(choice)].to_vec()),
        );
        if let Some(usage) = self.pending_usage.take() {
            chunk.insert("usage".to_string(), usage);
        }
        Frame::Chunk(Value::Object(chunk))
    }
}

/// 流式 tool_call 片段的字段整形：`function_call` → `function`，
/// 并剥掉 SOLO 专属的 `namespace` / `partial_arguments`（上游带着它们）。
fn normalize_stream_tool_calls(calls: Value) -> Value {
    let Value::Array(list) = calls else {
        return if calls.is_object() {
            Value::Array(vec![normalize_one_call(calls)])
        } else {
            json!([])
        };
    };
    Value::Array(
        list.into_iter()
            .filter(|call| call.is_object())
            .map(normalize_one_call)
            .collect(),
    )
}

fn normalize_one_call(call: Value) -> Value {
    let Value::Object(mut call) = call else {
        return call;
    };
    if let Some(function) = call.remove("function_call") {
        call.insert("function".to_string(), strip_solo_only_fields(function));
    } else if let Some(function) = call.get("function").cloned() {
        call.insert("function".to_string(), strip_solo_only_fields(function));
    }
    Value::Object(call)
}

fn strip_solo_only_fields(function: Value) -> Value {
    let Value::Object(mut function) = function else {
        return function;
    };
    function.remove("namespace");
    function.remove("partial_arguments");
    Value::Object(function)
}

pub fn aggregate_chunks(chunks: &[Value], requested: &str) -> Value {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut calls = Vec::new();
    let mut finish = "stop".to_string();
    let mut usage = None;
    for chunk in chunks {
        let choice = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first());
        let delta = choice.and_then(|choice| choice.get("delta"));
        if let Some(text) = delta
            .and_then(|delta| delta.get("content"))
            .and_then(Value::as_str)
        {
            content.push_str(text);
        }
        if let Some(text) = delta
            .and_then(|delta| delta.get("reasoning_content"))
            .and_then(Value::as_str)
        {
            reasoning.push_str(text);
        }
        if let Some(incoming) = delta.and_then(|delta| delta.get("tool_calls")) {
            merge_tool_calls(&mut calls, incoming.clone());
        }
        if let Some(reason) = choice
            .and_then(|choice| choice.get("finish_reason"))
            .and_then(Value::as_str)
            .filter(|reason| !reason.is_empty())
        {
            finish = reason.to_string();
        }
        if let Some(value) = chunk.get("usage") {
            usage = Some(value.clone());
        }
    }
    let mut message = json!({ "role": "assistant", "content": content });
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
        if finish == "stop" {
            finish = "tool_calls".to_string();
        }
    }
    let first = chunks.first();
    let mut response = json!({
        "id": first.and_then(|chunk| chunk.get("id")).cloned().unwrap_or(Value::Null),
        "object": "chat.completion",
        "created": first.and_then(|chunk| chunk.get("created")).cloned().unwrap_or(Value::Null),
        "model": requested,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
    });
    if let Some(usage) = usage {
        response["usage"] = usage;
    }
    response
}

/// 按 `index` 合并流式 tool_call 片段：`id` / `type` / `function.name` 直覆盖，
/// `function.arguments` **拼接**。
fn merge_tool_calls(calls: &mut Vec<Value>, incoming: Value) {
    let list: Vec<Value> = match incoming {
        Value::Array(items) => items,
        Value::Object(one) => vec![Value::Object(one)],
        _ => return,
    };
    for call in list {
        let Value::Object(call) = call else { continue };
        let index = call.get("index").and_then(Value::as_i64).unwrap_or(0);
        let slot = match calls
            .iter_mut()
            .find(|existing| existing.get("index").and_then(Value::as_i64).unwrap_or(0) == index)
        {
            Some(existing) => existing,
            None => {
                calls.push(json!({"index": index}));
                match calls.last_mut() {
                    Some(slot) => slot,
                    None => continue,
                }
            }
        };
        let Some(target) = slot.as_object_mut() else {
            continue;
        };
        if let Some(id) = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            target.insert("id".to_string(), json!(id));
        }
        if let Some(kind) = call
            .get("type")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            target.insert("type".to_string(), json!(kind));
        }
        let delta_function = call
            .get("function")
            .or_else(|| call.get("function_call"))
            .cloned()
            .map(strip_solo_only_fields)
            .and_then(|value| value.as_object().cloned());
        let Some(delta_function) = delta_function else {
            continue;
        };
        let merged = target
            .entry("function".to_string())
            .or_insert_with(|| json!({}));
        let Some(merged) = merged.as_object_mut() else {
            continue;
        };
        if let Some(name) = delta_function
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            merged.insert("name".to_string(), json!(name));
        }
        if let Some(arguments) = delta_function
            .get("arguments")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            let previous = merged
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default();
            merged.insert(
                "arguments".to_string(),
                json!(format!("{previous}{arguments}")),
            );
        }
    }
}
