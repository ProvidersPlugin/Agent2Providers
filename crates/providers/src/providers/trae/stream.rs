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

use serde_json::{Value, json};

use super::errors::{ErrorKind, error_event_fields, stream_error_kind};

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
    if data.trim().is_empty() {
        // 只有 event 行没有 data：仍算一个事件边界（done 之类可能不带载荷）。
        return Some(match event {
            "done" => SoloEvent::Done { finish_reason: String::new() },
            _ => SoloEvent::Other,
        });
    }
    let parsed: Value = serde_json::from_str(data).ok()?;
    Some(match event {
        "output" => SoloEvent::Output {
            response: parsed.get("response").and_then(Value::as_str).unwrap_or_default().to_string(),
            reasoning: parsed
                .get("reasoning_content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            tool_calls: parsed.get("tool_calls").cloned().filter(|value| !value.is_null()),
        },
        "token_usage" => SoloEvent::TokenUsage(parsed),
        "done" => SoloEvent::Done {
            finish_reason: parsed.get("finish_reason").and_then(Value::as_str).unwrap_or_default().to_string(),
        },
        "error" => {
            let (code, message) = error_event_fields(&parsed);
            SoloEvent::Error { code, message }
        }
        _ => SoloEvent::Other,
    })
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
            if self.event.is_empty() {
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
        event.filter(|event| !matches!(event, SoloEvent::Done { .. }))
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
                format!("event: error\ndata: {}", json!(format!("solo error code={code} msg={message}")))
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
    saw_done: bool,
    completed: bool,
}

impl SoloStream {
    pub fn new(id: String, created: i64) -> Self {
        Self { id, created, scanner: SseScanner::default(), pending_usage: None, saw_done: false, completed: false }
    }

    /// 喂一行，返回该行产生的帧。
    pub fn feed_line(&mut self, line: &str) -> Vec<Frame> {
        if self.completed { return Vec::new(); }
        let Some(event) = self.scanner.feed_line(line) else {
            return Vec::new();
        };
        self.apply(event)
    }

    fn apply(&mut self, event: SoloEvent) -> Vec<Frame> {
        match event {
            SoloEvent::Output { response, reasoning, tool_calls } => {
                let mut delta = serde_json::Map::new();
                if !response.is_empty() {
                    delta.insert("content".to_string(), json!(response));
                }
                if !reasoning.is_empty() {
                    delta.insert("reasoning_content".to_string(), json!(reasoning));
                }
                if let Some(calls) = tool_calls {
                    delta.insert("tool_calls".to_string(), normalize_stream_tool_calls(calls));
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
                let mut frames = vec![self.chunk(json!({}), Some(finish_reason))];
                if !sent { frames.push(Frame::Done); }
                frames
            }
            SoloEvent::Error { code, message } => {
                let kind = stream_error_kind(code, &message);
                // error 后仍读取可能到来的用量；只有完整 done 事件结束读取。
                vec![Frame::Error { code, message, kind }]
            }
            SoloEvent::Other => Vec::new(),
        }
    }

    pub fn completed(&self) -> bool { self.completed }

    pub fn flush_pending(&mut self) -> Vec<Frame> {
        let mut frames = match self.scanner.finish() {
            Some(event) => self.apply(event),
            None => Vec::new(),
        };
        if self.pending_usage.is_some() { frames.push(self.chunk(json!({}), None)); }
        frames
    }

    /// EOF 时保留已收到的尾数据与用量，结束标记只发一次。
    pub fn finish(&mut self) -> Vec<Frame> {
        let mut frames = self.flush_pending();
        if !self.saw_done { frames.push(Frame::Done); }
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
        chunk.insert("choices".to_string(), Value::Array([Value::Object(choice)].to_vec()));
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
        // 上游偶尔给单个对象；按参考实现的处理方式包成数组。
        return Value::Array(vec![normalize_one_call(calls)]);
    };
    Value::Array(list.into_iter().map(normalize_one_call).collect())
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

/// 一条流内错误（非流式聚合时作为失败返回）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamError {
    pub code: i64,
    pub message: String,
}

impl StreamError {
    /// `#[allow(dead_code)]`：分类在 `errors::stream_error_kind` 里按码判，
    /// 这个便捷方法留给调用方（当前都直接调那个函数）。
    #[allow(dead_code)]
    pub fn kind(&self) -> ErrorKind {
        stream_error_kind(self.code, &self.message)
    }

    /// 与参考实现逐字同形的文案（对拍时要能直接对照日志）。
    #[allow(dead_code)]
    pub fn to_text(&self) -> String {
        format!("solo error code={} msg={}", self.code, self.message)
    }
}

/// 非流式：把整条 SSE 折成一个 `chat.completion`。
///
/// 上游没有非流式端点（`stream` 被强制成 true），所以这一条是**唯一**的
/// 非流式实现路径，不是"流式的替代品"。
pub fn aggregate(text: &str, id: &str, created: i64) -> Result<Value, StreamError> {
    let mut scanner = SseScanner::default();
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut finish_reason = "stop".to_string();
    let mut usage: Option<Value> = None;
    let mut calls: Vec<Value> = Vec::new();
    let mut upstream_error: Option<StreamError> = None;

    // 与 Go 的 `ReadString('\n')` 对齐：按行喂，**EOF 那一次还会再喂一个空行**
    // （那一下把最后一个没被空行闭合的事件推出去）。少喂这一行，
    // 结尾没有空行的流就会丢掉最后一帧。
    for line in text.split('\n').chain(std::iter::once("")) {
        let Some(event) = scanner.feed_line(line) else {
            continue;
        };
        match event {
            SoloEvent::Output { response, reasoning: thought, tool_calls } => {
                content.push_str(&response);
                reasoning.push_str(&thought);
                if let Some(calls_value) = tool_calls {
                    merge_tool_calls(&mut calls, calls_value);
                }
            }
            SoloEvent::TokenUsage(payload) => usage = Some(payload),
            SoloEvent::Done { finish_reason: reason } => {
                if !reason.is_empty() {
                    finish_reason = reason;
                }
            }
            SoloEvent::Error { code, message } => upstream_error = Some(StreamError { code, message }),
            SoloEvent::Other => {}
        }
    }

    if let Some(error) = upstream_error {
        return Err(error);
    }
    let mut message = serde_json::Map::new();
    message.insert("role".to_string(), json!("assistant"));
    message.insert("content".to_string(), json!(content));
    if !reasoning.is_empty() {
        message.insert("reasoning_content".to_string(), json!(reasoning));
    }
    if !calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(calls));
    }
    let mut response = serde_json::Map::new();
    response.insert("id".to_string(), json!(id));
    response.insert("object".to_string(), json!("chat.completion"));
    response.insert("created".to_string(), json!(created));
    response.insert("model".to_string(), json!(""));
    response.insert(
        "choices".to_string(),
        json!([{"index": 0, "message": Value::Object(message), "finish_reason": finish_reason}]),
    );
    if let Some(usage) = usage {
        response.insert("usage".to_string(), usage);
    }
    Ok(Value::Object(response))
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
        let slot = match calls.iter_mut().find(|existing| {
            existing.get("index").and_then(Value::as_i64).unwrap_or(0) == index
        }) {
            Some(existing) => existing,
            None => {
                calls.push(json!({"index": index}));
                calls.last_mut().expect("刚推进去")
            }
        };
        let Some(target) = slot.as_object_mut() else {
            continue;
        };
        if let Some(id) = call.get("id").and_then(Value::as_str).filter(|value| !value.is_empty()) {
            target.insert("id".to_string(), json!(id));
        }
        if let Some(kind) = call.get("type").and_then(Value::as_str).filter(|value| !value.is_empty()) {
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
        let merged = target.entry("function".to_string()).or_insert_with(|| json!({}));
        let Some(merged) = merged.as_object_mut() else {
            continue;
        };
        if let Some(name) = delta_function.get("name").and_then(Value::as_str).filter(|value| !value.is_empty()) {
            merged.insert("name".to_string(), json!(name));
        }
        if let Some(arguments) = delta_function.get("arguments").and_then(Value::as_str).filter(|value| !value.is_empty()) {
            let previous = merged.get("arguments").and_then(Value::as_str).unwrap_or_default();
            merged.insert("arguments".to_string(), json!(format!("{previous}{arguments}")));
        }
    }
}

