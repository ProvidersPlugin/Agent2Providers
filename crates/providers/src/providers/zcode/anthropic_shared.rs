//! Anthropic 出站转换要用到的两个小工具（从网关 `protocol::anthropic` 摘出来的
//! 仅**出站方向**需要的部分）。
//!
//! ── 为什么只摘两个函数，而不是整份搬 `anthropic.rs` ──────────────
//! `anthropic.rs` 里 90% 是**入站**方向（Anthropic SSE / 消息体 → Chat），那条
//! 路在插件架构里由网关的翻译机做（`UpstreamKind::Anthropic` 已经在协议里，
//! 见 `plugin_host::rpc::upstream_response`）。插件只需要把 Chat 体**转成**
//! Anthropic 体，所以只需要 `DEFAULT_MAX_TOKENS` / `tool_result_parts` /
//! `parse_json_object` 这三个符号。
//!
//! ── 与网关那份的关系（改一处别忘了另一处）────────────────────
//! 三份实现逐字相同（函数体、兜底文案、`(empty)` 占位都在）。它们**不会**
//! 互相调用 —— 插件是独立进程，链接不到网关的 crate。差异会表现为「同一个
//! 请求在两条通道上转出不同的 Anthropic 体」，那是 parity 脚本要抓的东西。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use crate::json::{content_text, string_field};

/// Anthropic 的 `max_tokens` 是**必填**字段，客户端没给时用这个默认值
/// （与网关 `protocol::anthropic::DEFAULT_MAX_TOKENS` 同值）。
pub(super) const DEFAULT_MAX_TOKENS: i64 = 8192;

/// 工具结果拆出来的两部分（见 [`tool_result_parts`]）。
pub(super) struct ToolResultParts {
    /// 文本部分（可能为空串 —— 只有图片时由调用方决定补什么）。
    pub(super) text: String,
    /// 图片块，**已按调用方给的转换器转成目标协议的形态**。
    pub(super) images: Vec<Value>,
}

/// 工具结果内容 → 文本 + 图片块（网关同名函数的逐字副本）。
///
/// `convert` 是本方向「图片块 → 目标协议形态」的转换器（出站给
/// [`super::anthropic_outbound`] 的 `image_to_anthropic`）。转不出来的块在这里
/// 就被剔除，所以调用方拿到的 `images` 一定落得下去，不必再判一次。
pub(super) fn tool_result_parts(
    content: &Value,
    convert: fn(&Value) -> Option<Value>,
) -> ToolResultParts {
    let text_only = |text: String| ToolResultParts {
        text,
        images: Vec::new(),
    };
    match content {
        Value::String(text) => {
            if text.is_empty() {
                text_only("(empty)".to_string())
            } else {
                text_only(text.clone())
            }
        }
        Value::Null => text_only("(empty)".to_string()),
        Value::Array(parts) => {
            let images: Vec<Value> = parts
                .iter()
                .filter(|part| is_image_block(part))
                .filter_map(|part| convert(part))
                .collect();
            let text = content_text(content);
            if text.is_empty() && images.is_empty() {
                // 一个块都落不下来（Files API 的 `file` 源图片、只有
                // `search_result` / `document` 的结果）：保持拍平兜底，
                // 免得整段结果凭空消失。
                let raw = json_text(content);
                return text_only(if raw.is_empty() {
                    "(empty)".to_string()
                } else {
                    raw
                });
            }
            ToolResultParts { text, images }
        }
        other => {
            let raw = json_text(other);
            text_only(if raw.is_empty() {
                "(empty)".to_string()
            } else {
                raw
            })
        }
    }
}

/// 是不是图片块（Anthropic 的 `image` 与 Chat 的 `image_url` / `input_image`
/// 都认 —— 转不转得出来交给 `convert` 判）。
fn is_image_block(part: &Value) -> bool {
    matches!(
        string_field(part, "type").to_lowercase().as_str(),
        "image" | "image_url" | "input_image"
    )
}

/// 参数表归一：不是对象就包一层 `{"value": ...}`，保证 `input` 是对象。
///
/// 上游（Anthropic）要求 `tool_use.input` 必须是对象；客户端把参数写成 JSON
/// 字符串是常见形态（Chat 的 `arguments` 就是字符串），所以这里既解字符串、
/// 也兜住解不出来的情况。
pub(super) fn parse_json_object(value: &Value) -> Value {
    if value.is_object() {
        return value.clone();
    }
    let text = json_text(value);
    if text.is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(parsed) if parsed.is_object() => parsed,
        // 解析不了（上游给了非 JSON 的参数）：包一层，保证 input 是对象
        _ => json!({ "value": value.clone() }),
    }
}

/// JSON 文本化：字符串取原文，其余走 `to_string`（与网关同口径）。
fn json_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

// `content_parts` 在本文件里没有直接调用方，但导出给上层用同一份实现时
// 不必再写一遍 —— 它就是 `json::content_parts` 的重导出。
#[allow(unused_imports)]
pub(super) use crate::json::content_parts as parts_of;
