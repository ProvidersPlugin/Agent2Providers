//! Antigravity 请求信封 / 响应翻译用到的**协议辅助件**（从网关 `core::protocol`
//! 搬来的最小集合）。
//!
//! ── 为什么只搬这几件 ────────────────────────────────────────
//! 网关的 `core::protocol` 是一整套「协议互译」库（Responses / Anthropic 双向
//! 转换、工具计划、出站清洗……），那些属于**网关编排层**。插件这边只需要
//! Antigravity 这条链真正用到的那几个取值函数与 SSE 行缓冲 —— 搬全了会把
//! 一半用不上的代码带进插件。
//!
//! 语义逐字保留（`string_value` 是 JS 的 `String(value)`、`SseLineBuffer` 按
//! 字节缓冲），这样 `antigravity_outbound` / `antigravity_stream` 两个文件与
//! 网关那份的 diff 里只剩真正的迁移改动。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。

#![allow(dead_code)]

use serde_json::Value;

/// 转换失败的原因（与网关 `protocol::responses::ConvertError` 同形）。
pub type ConvertError = String;

/// JS 的 `String(value)`：非字符串也照转（`null` → 空串）。
pub fn string_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 取一个对象成员并转成字符串（缺失 → 空串）。
pub fn string_field(value: &Value, key: &str) -> String {
    value.get(key).map(string_value).unwrap_or_default()
}

/// 内容 → 纯文本（字符串原样；块数组按 `text` 拼；其余 `to_string`）。
pub fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => {
            let mut out = String::new();
            for part in parts {
                if let Some(text) = part.as_str() {
                    out.push_str(text);
                    continue;
                }
                let kind = part.get("type").map(string_value).unwrap_or_default();
                let kind = kind.to_lowercase();
                if matches!(kind.as_str(), "image" | "image_url" | "input_image") {
                    continue;
                }
                if matches!(kind.as_str(), "text" | "input_text" | "output_text") {
                    out.push_str(&string_field(part, "text"));
                    continue;
                }
                let text = string_field(part, "text");
                if text.is_empty() {
                    out.push_str(&part.to_string());
                } else {
                    out.push_str(&text);
                }
            }
            out
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 内容块数组（非数组时给空切片）。
pub fn content_parts(content: &Value) -> &[Value] {
    match content.as_array() {
        Some(parts) => parts.as_slice(),
        None => &[],
    }
}

/// JS 真值判定（`Boolean(x)`）。
pub fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// 任意 JSON → 文本（字符串原样，其余序列化）。
pub fn json_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// 生成一个带前缀的随机 id（`msg_<32位十六进制>` 形态）。
pub fn random_id(prefix: &str) -> String {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        return format!("{prefix}_{:x}", crate::logging::now_ms());
    }
    let mut out = String::with_capacity(prefix.len() + 1 + 32);
    out.push_str(prefix);
    out.push('_');
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// 拼一条 chat SSE 帧（`data: {...}\n\n`）。
///
/// 返回 `bytes::Bytes` 与网关那份逐字一致：翻译机的下游（`antigravity_stream`
/// 的 `tool_call_frames` / `delta_frame`）用的就是 `Bytes`，改类型会让搬来的
/// 每一处都要 `.into()`。
pub fn chat_frame(value: &Value) -> bytes::Bytes {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    bytes::Bytes::from(format!("data: {text}\n\n"))
}

/// 跨 chunk 的 SSE 行缓冲（只取 `data:` 载荷，`[DONE]` 给 `None`）。
///
/// 缓冲按**字节**而不是 String：思考内容里中文占大头，而分片完全可能把一个
/// 3 字节的汉字切成两半，按 String 缓冲会把那一半解码成 U+FFFD（内容损坏）。
#[derive(Default)]
pub struct SseLineBuffer {
    tail: Vec<u8>,
}

impl SseLineBuffer {
    pub fn new() -> Self {
        Self { tail: Vec::new() }
    }

    /// 吃一段字节，吐出其中**完整**的 `data:` 载荷（0..n 个）。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Option<String>> {
        let mut out: Vec<Option<String>> = Vec::new();
        self.tail.extend_from_slice(chunk);
        let mut start = 0usize;
        while let Some(offset) = self.tail[start..].iter().position(|byte| *byte == b'\n') {
            let end = start + offset;
            let line = String::from_utf8_lossy(&self.tail[start..end]).to_string();
            Self::take_line(&line, &mut out);
            start = end + 1;
        }
        self.tail.drain(..start);
        out
    }

    /// 流结束：把残留的半行也当一帧处理。
    ///
    /// ── 为什么这里**要**冲刷 ────────────────────────────────────
    /// 我们是**翻译器**，残缺的 JSON 会被 `serde_json` 拒绝并静默丢弃（不会
    /// 构造出非法帧），而「上游发完最后一帧却没带收尾换行」这种情况能因此被
    /// 正确识别 —— 少了这一步，流的最后一个内容分片会凭空消失。
    pub fn finish(&mut self) -> Vec<Option<String>> {
        let mut out: Vec<Option<String>> = Vec::new();
        if self.tail.is_empty() {
            return out;
        }
        let line = String::from_utf8_lossy(&self.tail).to_string();
        self.tail.clear();
        Self::take_line(&line, &mut out);
        out
    }

    /// 处理一行：只取 `data:` 行，跳过事件名 / 注释 / 空行。
    fn take_line(line: &str, out: &mut Vec<Option<String>>) {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let Some(rest) = line.strip_prefix("data:") else {
            return;
        };
        let data = rest.trim();
        if data.is_empty() {
            return;
        }
        if data == "[DONE]" {
            out.push(None);
            return;
        }
        out.push(Some(data.to_string()));
    }
}
