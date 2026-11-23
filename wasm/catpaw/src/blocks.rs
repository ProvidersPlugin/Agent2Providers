//! Chat 内容块和历史工具材料的纯转换；图片只接受当前已接通的用户输入形态。
//! 工具参数只作为 JSON 文本提交；不执行、不补结果、不从内容中取得控制权限。

use agent2api_wasm_sdk::{json, Result, Value};

use crate::{errors, tools};

const MAX_BLOCKS: usize = 256;
const MAX_CALLS: usize = 256;
const MAX_ARGUMENT_BYTES: usize = 256 * 1024;
const MAX_RESULT_BYTES: usize = 1024 * 1024;

pub(super) struct ToolCall {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) arguments: String,
}

pub(super) fn content(value: Option<&Value>) -> Result<Vec<Value>> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![json!({ "type": "text", "text": text })]),
        Some(Value::Array(items)) if items.len() <= MAX_BLOCKS =>
            items.iter().map(text_block).collect(),
        Some(Value::Array(_)) => Err(errors::invalid("CatPaw 单条消息内容块超过 256 项")),
        _ => Err(errors::invalid("CatPaw 消息 content 必须是文本、内容块数组或 null")),
    }
}

fn text_block(value: &Value) -> Result<Value> {
    if let Some(text) = value.as_str() { return Ok(json!({ "type": "text", "text": text })); }
    let object = value.as_object().ok_or_else(|| errors::invalid("CatPaw 内容块必须是对象或文本"))?;
    match object.get("type").and_then(Value::as_str) {
        Some("image_url") => return crate::images::block(value),
        Some("input_image" | "image") => return Err(image_error()),
        Some("text" | "output_text") => {},
        _ => return Err(errors::invalid("CatPaw 当前对话转换只接受文本内容块，未忽略未知内容")),
    }
    let text = object.get("text").and_then(Value::as_str)
        .ok_or_else(|| errors::invalid("CatPaw 文本内容块缺少 text"))?;
    let mut block = json!({ "type": "text", "text": text });
    // 旧协议块级别为蛇形覆盖驼峰，非文本值不能静默忽略。
    for key in ["reasoningContent", "reasoning_content"] {
        if let Some(value) = object.get(key).filter(|value| !value.is_null()) {
            let value = value.as_str().ok_or_else(|| errors::invalid("CatPaw 思考内容必须是文本"))?;
            block["reasoningContent"] = json!(value);
        }
    }
    Ok(block)
}

pub(super) fn tool_calls(message: &Value) -> Result<Vec<ToolCall>> {
    // 输入是规范 Chat，不把另一个方言的字段偷偷当成空工具集。
    for key in ["toolCalls", "function_call"] {
        if message.get(key).is_some_and(|value| !value.is_null()) {
            return Err(errors::invalid("CatPaw 历史工具调用请使用规范 tool_calls 字段"));
        }
    }
    let items = match message.get("tool_calls") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(items)) if items.len() <= MAX_CALLS => items,
        _ => return Err(errors::invalid("CatPaw assistant.tool_calls 必须是不超过 256 项的数组")),
    };
    let mut calls = Vec::with_capacity(items.len());
    let mut ids = std::collections::BTreeSet::new();
    for call in items {
        if call["type"].as_str() != Some("function") {
            return Err(errors::invalid("CatPaw 历史 tool_call 只接受 type=function"));
        }
        let function = call["function"].as_object()
            .ok_or_else(|| errors::invalid("CatPaw 历史 tool_call 缺少 function 对象"))?;
        let id = call["id"].as_str().ok_or_else(|| errors::invalid("CatPaw 历史 tool_call 缺少 id"))?;
        tools::validate_call_id(id)?;
        if !ids.insert(id) { return Err(errors::invalid("CatPaw assistant 存在重复的 tool_call_id")); }
        let name = function.get("name").and_then(Value::as_str)
            .ok_or_else(|| errors::invalid("CatPaw 历史 tool_call 缺少 function.name"))?;
        tools::validate_name(name)?;
        let arguments = arguments(function.get("arguments"))?;
        calls.push(ToolCall { id: id.to_string(), name: name.to_string(), arguments });
    }
    Ok(calls)
}

fn arguments(value: Option<&Value>) -> Result<String> {
    match value {
        // 不把缺失或空文本变成“已经知道是空对象”的工具调用。
        Some(Value::String(text)) => {
            if text.len() > MAX_ARGUMENT_BYTES || text.trim().is_empty() {
                return Err(errors::invalid("CatPaw 工具 arguments 必须是有界、完整的 JSON 对象文本"));
            }
            // 先限原文本物理嵌套，避免 serde 的默认深度前就发生大栈遍历。
            raw_depth(text)?;
            let parsed: Value = serde_json::from_str(text)
                .map_err(|_| errors::invalid("CatPaw 工具 arguments 不是完整 JSON"))?;
            if !parsed.is_object() { return Err(errors::invalid("CatPaw 工具 arguments 必须是 JSON 对象")); }
            raw_keys(text)?;
            tools::validate_json(&parsed, false)?;
            // 字符串原样保留，不能重排参数后破坏后续历史一致性。
            Ok(text.clone())
        },
        Some(value @ Value::Object(_)) => {
            tools::validate_json(value, false)?;
            serde_json::to_string(value).map_err(|_| errors::invalid("CatPaw 工具 arguments 无法编码"))
        },
        _ => Err(errors::invalid("CatPaw 工具 arguments 必须是 JSON 对象或完整对象文本")),
    }
}

/// 不接受嵌套图片资源作为工具结果；其他结构只编码为文本，不解释其中的动作。
pub(super) fn tool_result(message: &Value) -> Result<String> {
    let value = message.get("content")
        .ok_or_else(|| errors::invalid("CatPaw tool 消息缺少实际 content，未补造工具结果"))?;
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Array(items) if items.len() <= MAX_BLOCKS => {
            reject_images(value)?;
            if items.iter().all(Value::is_string) {
                items.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n")
            } else if items.iter().all(|item| matches!(item["type"].as_str(), Some("text" | "output_text"))
                && item["text"].is_string()) {
                items.iter().filter_map(|item| item["text"].as_str()).collect::<Vec<_>>().join("\n")
            } else {
                tools::validate_json(value, false)?;
                serde_json::to_string(value).map_err(|_| errors::invalid("CatPaw 工具结果无法编码"))?
            }
        },
        Value::Array(_) => return Err(errors::invalid("CatPaw 工具结果内容块超过 256 项")),
        _ => return Err(errors::invalid("CatPaw tool.content 必须包含用户实际提供的文本或内容块数组")),
    };
    if text.len() > MAX_RESULT_BYTES {
        return Err(errors::invalid("CatPaw 单条工具结果超过 1 MiB，未截断发送"));
    }
    Ok(text)
}

pub(super) fn reject_images(value: &Value) -> Result<()> {
    // 调用方先执行 prepare 的全局深度/节点预算；这里只识别结构化图片块。
    match value {
        Value::Array(items) => { for item in items { reject_images(item)?; } },
        Value::Object(object) => {
            if matches!(object.get("type").and_then(Value::as_str), Some("image_url" | "input_image" | "image")) {
                return Err(image_error());
            }
            for item in object.values() { reject_images(item)?; }
        },
        _ => {},
    }
    Ok(())
}

fn image_error() -> agent2api_wasm_sdk::Error {
    errors::invalid("CatPaw 图片只支持用户消息中的已接通图片格式，不接受非用户消息或工具结果中的图片；未丢弃图片改发纯文字")
}

/// Value 会折叠重名键，而原 arguments 文本要原样发送；先扫描已通过 JSON 语法校验的
/// 文本，逐个对象拒绝解码后重复键，防止校验视图与上游实际收到的材料不同。
fn raw_keys(text: &str) -> Result<()> {
    let bytes = text.as_bytes();
    let mut stack: Vec<Option<std::collections::BTreeSet<String>>> = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        match bytes[offset] {
            b'{' => { stack.push(Some(std::collections::BTreeSet::new())); offset += 1; },
            b'[' => { stack.push(None); offset += 1; },
            b'}' | b']' => { stack.pop(); offset += 1; },
            b'"' => {
                let start = offset;
                offset += 1;
                while offset < bytes.len() {
                    match bytes[offset] {
                        b'\\' => offset = offset.saturating_add(2),
                        b'"' => { offset += 1; break; },
                        _ => offset += 1,
                    }
                }
                let mut after = offset;
                while bytes.get(after).is_some_and(u8::is_ascii_whitespace) { after += 1; }
                if bytes.get(after) != Some(&b':') { continue; }
                let Some(Some(keys)) = stack.last_mut() else {
                    return Err(errors::invalid("CatPaw 工具 arguments 的对象键位置无效"));
                };
                let quoted = text.get(start..offset)
                    .ok_or_else(|| errors::invalid("CatPaw 工具 arguments 的对象键无效"))?;
                let key: String = serde_json::from_str(quoted)
                    .map_err(|_| errors::invalid("CatPaw 工具 arguments 的对象键无效"))?;
                if !keys.insert(key) {
                    return Err(errors::invalid("CatPaw 工具 arguments 存在重复对象键，未以折叠后的视图替代原文"));
                }
            },
            _ => offset += 1,
        }
    }
    Ok(())
}

fn raw_depth(text: &str) -> Result<()> {
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for byte in text.bytes() {
        if quoted {
            if escaped { escaped = false; }
            else if byte == b'\\' { escaped = true; }
            else if byte == b'"' { quoted = false; }
        } else {
            match byte {
                b'"' => quoted = true,
                b'{' | b'[' => {
                    depth += 1;
                    if depth > 64 { return Err(errors::invalid("CatPaw 工具 arguments 的 JSON 嵌套过深")); }
                },
                b'}' | b']' => depth = depth.saturating_sub(1),
                _ => {},
            }
        }
    }
    Ok(())
}
