//! 只构造客户端工具声明，不执行工具、不建立跨请求索引。
//! 字段沿用旧 CatPaw tools.rs；除语义深度外另限物理深度/节点，组合键不能绕过预算。

use agent2api_wasm_sdk::{json, Result, Value};
use std::collections::BTreeSet;

use crate::errors;

const MAX_TOOLS: usize = 256;
const MAX_DESCRIPTION_CHARS: usize = 8000;
const MAX_DEPTH: usize = 32;
const MAX_PHYSICAL_DEPTH: usize = 64;
const MAX_NODES: usize = 32_768;
const MAX_TOOL_BYTES: usize = 256 * 1024;
const MAX_TOOLS_BYTES: usize = 2 * 1024 * 1024;

/// 这是客户端要求，不是上游已经满足该要求的回执。
#[derive(Clone, PartialEq, Eq)]
pub(super) enum ToolChoice {
    Auto,
    None,
    Required,
    Function(String),
}

pub(super) struct PreparedTools {
    pub(super) choice: ToolChoice,
    pub(super) configs: Vec<Value>,
    pub(super) available: Vec<String>,
    /// 旧协议没有对应字段；保留原意，后续执行器不得宣称已强制其并行行为。
    pub(super) parallel_requested: Option<bool>,
}

pub(super) fn prepare(body: &Value) -> Result<PreparedTools> {
    let choice = choice(body.get("tool_choice"))?;
    let items = match body.get("tools") {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(items)) if items.len() <= MAX_TOOLS => items.as_slice(),
        _ => return Err(errors::invalid("CatPaw tools 必须是不超过 256 项的数组")),
    };
    let parallel_requested = match body.get("parallel_tool_calls") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(value)) => Some(*value),
        _ => return Err(errors::invalid("parallel_tool_calls 必须是布尔值")),
    };
    let mut names = BTreeSet::new();
    let mut configs = Vec::with_capacity(items.len());
    let mut available = Vec::with_capacity(items.len());
    let mut bytes = 0usize;
    for tool in items {
        if tool["type"].as_str() != Some("function") {
            return Err(errors::invalid("CatPaw tools 只接受 type=function 的工具"));
        }
        let function = tool["function"].as_object()
            .ok_or_else(|| errors::invalid("CatPaw 工具缺少 function 对象"))?;
        match function.get("strict") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => {},
            Some(Value::Bool(true)) => return Err(errors::invalid("CatPaw 暂不支持工具 strict=true")),
            _ => return Err(errors::invalid("工具 strict 必须是布尔值")),
        }
        let name = function.get("name").and_then(Value::as_str)
            .ok_or_else(|| errors::invalid("CatPaw 工具缺少名称"))?;
        validate_name(name)?;
        if !names.insert(name) { return Err(errors::invalid("CatPaw tools 存在重复名称")); }
        let default_schema = json!({ "type": "object", "properties": {} });
        let schema = function.get("parameters").filter(|value| !value.is_null())
            .unwrap_or(&default_schema);
        if !schema.is_object() {
            return Err(errors::invalid("CatPaw 工具 parameters 必须是 JSON Schema 对象"));
        }
        validate_json(schema, true)?;
        let description = match function.get("description") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) if text.chars().count() <= MAX_DESCRIPTION_CHARS => Some(text),
            Some(Value::String(_)) => return Err(errors::invalid("CatPaw 工具说明超过 8000 字符，未截断发送")),
            _ => return Err(errors::invalid("CatPaw 工具 description 必须是文本")),
        };
        let mut config = json!({ "name": name, "enable": true, "fromClient": true,
            "inputSchema": schema });
        if let Some(description) = description { config["description"] = json!(description); }
        bytes = bytes.checked_add(crate::prepare::json_bytes(&config, MAX_TOOL_BYTES)?)
            .ok_or_else(|| errors::invalid("CatPaw 工具材料超过大小限制"))?;
        if bytes > MAX_TOOLS_BYTES {
            return Err(errors::invalid("CatPaw 工具材料超过 2 MiB，未截断发送"));
        }
        // 未选中的工具也须校验，不能用 tool_choice=none 隐藏非法输入。
        let selected = match &choice {
            ToolChoice::None => false,
            ToolChoice::Auto | ToolChoice::Required => true,
            ToolChoice::Function(wanted) => wanted == name,
        };
        if selected { configs.push(config); available.push(name.to_string()); }
    }
    match &choice {
        ToolChoice::Required if configs.is_empty() =>
            return Err(errors::invalid("tool_choice=required 至少需要一个 function 工具")),
        ToolChoice::Function(_) if configs.is_empty() =>
            return Err(errors::invalid("tool_choice 指定的工具未在本次 tools 中声明")),
        _ => {},
    }
    Ok(PreparedTools { choice, configs, available, parallel_requested })
}

fn choice(value: Option<&Value>) -> Result<ToolChoice> {
    match value {
        None | Some(Value::Null) => Ok(ToolChoice::Auto),
        Some(Value::String(value)) if value == "auto" => Ok(ToolChoice::Auto),
        Some(Value::String(value)) if value == "none" => Ok(ToolChoice::None),
        Some(Value::String(value)) if value == "required" => Ok(ToolChoice::Required),
        Some(Value::Object(value)) if value.get("type").and_then(Value::as_str) == Some("function") => {
            let name = value.get("function").and_then(|value| value.get("name"))
                .and_then(Value::as_str).ok_or_else(|| errors::invalid("tool_choice 缺少 function.name"))?;
            validate_name(name)?;
            Ok(ToolChoice::Function(name.to_string()))
        },
        _ => Err(errors::invalid("tool_choice 只支持 auto、none、required 或指定 function")),
    }
}

pub(super) fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 128
        || !name.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte)) {
        return Err(errors::invalid("CatPaw 工具名称必须为不超过 128 字节的字母、数字、下划线、点或连字符"));
    }
    Ok(())
}

pub(super) fn validate_call_id(id: &str) -> Result<()> {
    if id.trim().is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
        return Err(errors::invalid("CatPaw tool_call_id 为空、过长或包含控制字符"));
    }
    Ok(())
}

/// schema 的组合键不计语义层级，但每个实际 JSON 容器都计物理深度和节点数。
/// arguments/result 则不豁免 anyOf 等普通数据键。
pub(super) fn validate_json(value: &Value, schema: bool) -> Result<()> {
    let mut nodes = 0usize;
    walk(value, schema, 0, 0, &mut nodes)?;
    crate::prepare::json_bytes(value, MAX_TOOL_BYTES)?;
    Ok(())
}

fn walk(value: &Value, schema: bool, depth: usize, physical: usize, nodes: &mut usize) -> Result<()> {
    *nodes += 1;
    if *nodes > MAX_NODES || depth > MAX_DEPTH || physical > MAX_PHYSICAL_DEPTH {
        return Err(errors::invalid("CatPaw 工具 JSON 的深度或节点数超过限制"));
    }
    match value {
        Value::Array(items) => {
            for item in items { walk(item, schema, depth + 1, physical + 1, nodes)?; }
        },
        Value::Object(object) => {
            for (key, item) in object {
                if matches!(key.as_str(), "__proto__" | "constructor" | "prototype") {
                    return Err(errors::invalid("CatPaw 工具 JSON 包含不允许的原型字段"));
                }
                if schema && matches!(key.as_str(), "anyOf" | "oneOf" | "allOf" | "not") {
                    if let Value::Array(branches) = item {
                        *nodes += 1;
                        if *nodes > MAX_NODES || physical + 1 > MAX_PHYSICAL_DEPTH {
                            return Err(errors::invalid("CatPaw 工具 JSON 的深度或节点数超过限制"));
                        }
                        for branch in branches { walk(branch, schema, depth, physical + 2, nodes)?; }
                    } else { walk(item, schema, depth, physical + 1, nodes)?; }
                } else { walk(item, schema, depth + 1, physical + 1, nodes)?; }
            }
        },
        _ => {},
    }
    Ok(())
}
