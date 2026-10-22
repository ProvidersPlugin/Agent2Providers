//! JSON 取值的小工具（**从网关 `core::protocol` 搬来的三件**）。
//!
//! ── 为什么只搬这三个 ────────────────────────────────────────
//! 网关的 `core::protocol` 有一整套「协议互译」的东西（Responses / Anthropic
//! 双向转换、工具计划、SSE 帧组装……）。那些属于**网关的编排层**，插件不需要
//! —— 插件只负责「怎么构造这一次上游请求」与「这次响应说的是哪套协议」。
//!
//! 但各家适配器确实反复用到这三个取值函数，而且它们的语义很关键：
//! `string_value` 是 JS 的 `String(value)`（非字符串也照转，`null` → 空串），
//! 与 `as_str().unwrap_or("")` 的区别是**数字不会被静默丢掉**。搬一份过来
//! 让各家的取值链逐字保留，比每家自己写一遍更不容易分叉。

use serde_json::Value;

/// JS 的 `String(value)`：非字符串也照转（`null` → 空串）。
///
/// 适配层到处要按「客户端给的可能是任何 JSON 类型」处理（手写的请求体里
/// `model` 可能是数字、`name` 可能是对象），而 JS 参考实现处处是 `stringValue`。
pub fn string_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 取一个对象成员并转成字符串（缺失 → 空串）
pub fn string_field(value: &Value, key: &str) -> String {
    value.get(key).map(string_value).unwrap_or_default()
}

/// 内容块数组（非数组时给空切片语义）——大量调用点要遍历它
/// 内容 → 纯文本（字符串原样；块数组按 `text` 拼；其余 `to_string`）。
///
/// 与网关 `protocol::content_text` 同口径：图片块贡献空串（它的正文在别处），
/// 未知块退回整块 JSON —— 宁可把结构塞进文本，也不静默丢内容。
pub fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|part| match part {
                Value::String(text) => text.clone(),
                other => {
                    let kind = string_field(other, "type");
                    if matches!(kind.as_str(), "image" | "image_url" | "input_image") {
                        return String::new();
                    }
                    let text = string_field(other, "text");
                    if text.is_empty() {
                        other.to_string()
                    } else {
                        text
                    }
                }
            })
            .collect::<Vec<_>>()
            .join(""),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub fn content_parts(content: &Value) -> &[Value] {
    match content.as_array() {
        Some(parts) => parts.as_slice(),
        None => &[],
    }
}

/// 把 `f64` 转成 JSON 数字（整数走 `i64`，非有限值给 `Null`）。
///
/// 与网关 `account_store::state::json_number` 同口径：它被各家「公开视图」
/// 用来输出 `tokenExpiresAt` 这类毫秒时间戳 —— 直接 `json!(f64)` 会让界面
/// 看到 `1780000000000.0`，而整数形态才是那边一直在读的形状。
pub fn json_number(value: f64) -> Value {
    if !value.is_finite() {
        return Value::Null;
    }
    if value.fract() == 0.0 && value.abs() < i64::MAX as f64 {
        return Value::from(value as i64);
    }
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// Node 的 `text.slice(0, max)`：按**字符**截断（不是字节）。
///
/// 从网关 `account_store::store_util::truncate_text` 搬来：搬过来的各家
/// 在拼错误文案时用它截上游响应体（那些文案会进日志与界面）。
/// 按字符而不是按字节截断的理由是中文响应体 —— 按字节切会切在半个汉字上，
/// 拼进 `format!` 后是乱码，而且 `&s[..n]` 在非字符边界上会 panic
/// （release 是 `panic=abort`，会带走整个插件进程）。
pub fn truncate_text(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}
