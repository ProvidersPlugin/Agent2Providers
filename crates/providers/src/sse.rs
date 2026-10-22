//! 上游 SSE 帧的解析与重组工具。
//!
//! ── 为什么插件里需要这个（网关已经有 `upstream::sse`）──────────
//! 网关那套是**转发链**用的（合并 reasoning、改写 model、丢保活帧），跑在
//! 网关进程里。而插件的 `conversation` 是**自己**消费上游 SSE、自己推
//! chat 帧 —— 它需要的是「逐帧拆开 → 取某个字段 → 重新打包」，两件事的
//! 关注点不同。
//!
//! ── 三种常见上游形态 ────────────────────────────────────────
//!   1. 标准 OpenAI：`data: {"choices":[{"delta":{...}}]}` + `data: [DONE]`；
//!   2. 自有信封（OrcaTerm 的 `{"content":"…","reasoning":true}`、
//!      Loomy 的直通、ChatGLM 的混合快照）—— 用 [`SseLine`] 逐行拆；
//!   3. 非 SSE（NDJSON）：一行一个 JSON，没有 `data:` 前缀 —— 用
//!      [`json_lines`]。
//!
//! ── 为什么提供一个 `ChatChunk` 构造器 ────────────────────────
//! 每家把上游字段翻成 OpenAI 帧时都要拼同一个骨架（`id` / `object` /
//! `created` / `choices[0].delta`）。各写一份的下场是「某家漏了 `index`」
//! 这类不一致，而客户端对缺字段的容忍度各不相同。
//!
//! ── 关于 dead_code 豁免 ────────────────────────────────────
//! 这是一个**共用工具箱**：`SseLine` / `LineBuffer` 是给还没搬完的那几家
//! （NDJSON、混合快照帧）准备的。在它们搬进来之前必然有未调用的项 ——
//! 逐个删掉再逐个加回来只会制造无意义的 diff。
#![allow(dead_code)]

use serde_json::{json, Value};

/// 一行 SSE 的解析结果。
#[derive(Clone, Debug, PartialEq)]
pub enum SseLine {
    /// `data: <payload>`（`payload` 已去掉前缀与首尾空白）。
    Data(String),
    /// 空行（帧分隔符）或注释行（`:` 开头的心跳）。
    Blank,
    /// `event:` / `id:` / `retry:` 这类其它字段（值原样保留）。
    Field(String, String),
}

impl SseLine {
    /// 解析一行（不含行尾换行符）。
    pub fn parse(line: &str) -> Self {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() || trimmed.starts_with(':') {
            return Self::Blank;
        }
        match trimmed.split_once(':') {
            Some(("data", rest)) => Self::Data(rest.trim_start().to_string()),
            Some((name, rest)) => Self::Field(name.trim().to_string(), rest.trim_start().to_string()),
            // 没有冒号的行：SSE 规范里是「字段名 + 空值」，实际极少见
            None => Self::Field(trimmed.to_string(), String::new()),
        }
    }

    /// 是 `data:` 行就取出载荷。
    pub fn data(&self) -> Option<&str> {
        match self {
            Self::Data(payload) => Some(payload),
            _ => None,
        }
    }
}

/// 一个增量式的 SSE 行拆分器。
///
/// ── 为什么需要增量 ──────────────────────────────────────────
/// 上游的 TCP 分片与 SSE 行**不对齐**：一次 `bytes_stream().next()` 可能拿到
/// 半行，也可能拿到三行半。每家用 `String::find('\n')` 各写一遍的下场是
/// 「某家忘了保留最后半行」，症状是「长回答偶尔少最后一句」。
#[derive(Default)]
pub struct LineBuffer {
    buffer: String,
}

impl LineBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一段字节，返回**完整**的行（最后半行留在内部）。
    ///
    /// 用 `from_utf8_lossy`：上游偶尔会切在多字节字符中间，严格解码会让
    /// 整条流报错。丢一个字符比断一条流好。
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
        let mut lines = Vec::new();
        while let Some(index) = self.buffer.find('\n') {
            let line = self.buffer[..index].to_string();
            self.buffer.drain(..=index);
            lines.push(line);
        }
        lines
    }

    /// 收尾：把残留的半行也交出来（上游可能在最后一行没写换行）。
    pub fn finish(&mut self) -> Option<String> {
        if self.buffer.is_empty() {
            return None;
        }
        let rest = std::mem::take(&mut self.buffer);
        Some(rest)
    }
}

/// 把一段文本按行拆成 JSON（NDJSON 用）。
pub fn json_lines(text: &str) -> impl Iterator<Item = Value> + '_ {
    text.lines().filter_map(|line| {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        serde_json::from_str::<Value>(trimmed).ok()
    })
}

/// 拼一个 OpenAI chat.completion.chunk 的骨架。
///
/// `delta` 是 `choices[0].delta` 的内容（`{"content":"…"}` 这类）。
/// `finish` 非空时填 `finish_reason`（终止帧用）。
pub fn chat_chunk(id: &str, model: &str, created: i64, delta: Value, finish: Option<&str>) -> Value {
    json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish,
        }],
    })
}

/// 把一帧 chat.chunk 序列化成 SSE 行（带 `data: ` 前缀与空行结尾）。
pub fn sse_bytes(value: &Value) -> Vec<u8> {
    format!("data: {}\n\n", value).into_bytes()
}

/// 终止帧 `data: [DONE]`。
pub fn done_bytes() -> Vec<u8> {
    b"data: [DONE]\n\n".to_vec()
}

/// 流内错误帧（客户端据此知道这条流是失败的）。
///
/// 形态与网关 `ForwardStream` 的错误帧一致（`{"error":{...}}` + `[DONE]`）：
/// 客户端此时已经收到 200 与部分内容，唯一能告诉它「失败」的办法就是
/// 在流里写一条错误帧。
pub fn error_bytes(message: &str) -> Vec<u8> {
    let frame = json!({
        "error": {
            "message": message,
            "type": "proxy_error",
        }
    });
    sse_bytes(&frame)
}

/// 从 chat 帧里取增量文本（`delta.content`）。
pub fn content_of(chunk: &Value) -> Option<&str> {
    chunk
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("delta"))
        .and_then(|delta| delta.get("content"))
        .and_then(Value::as_str)
}

/// 从 chat 帧里取思考增量（`delta.reasoning_content` / `delta.reasoning`）。
///
/// 两个键都认：DeepSeek 系用前者，Cline 免费池用后者（网关的聚合器同一口径）。
pub fn reasoning_of(chunk: &Value) -> Option<&str> {
    let delta = chunk
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("delta"))?;
    delta
        .get("reasoning_content")
        .and_then(Value::as_str)
        .or_else(|| delta.get("reasoning").and_then(Value::as_str))
}
