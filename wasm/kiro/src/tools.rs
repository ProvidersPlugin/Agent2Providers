//! Kiro工具名、ID与schema的纯数据规则，源自原生request.rs；回包恢复客户端原名。
//! 相同规整名对应不同原名时拒绝，不让两个工具静默合成一个。

use crate::util;
use agent2api_wasm_sdk::{json, Error, Result, Value};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub fn name(name: &str) -> String {
    if !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || b"_-".contains(&ch))
    {
        return name.into();
    }
    let hash = util::hash(name, 6);
    let prefix: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .take(64 - 1 - hash.len())
        .collect();
    format!("{prefix}_{hash}")
}
pub fn id(id: &str) -> String {
    if !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || b"_.:-".contains(&ch))
    {
        return id.into();
    }
    format!(
        "t_{}",
        URL_SAFE_NO_PAD
            .encode(Sha256::digest(id.as_bytes()))
            .chars()
            .take(32)
            .collect::<String>()
    )
}
pub fn schema(value: Option<&Value>) -> Result<Value> {
    let mut value = value
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !value.is_object() {
        return Err(Error::http(400, "Kiro 工具参数结构必须为对象"));
    }
    match value.get("type") {
        None => value["type"] = json!("object"),
        Some(Value::String(kind)) if kind == "object" => {}
        Some(Value::Array(kinds))
            if !kinds.is_empty() && kinds.iter().all(|kind| kind == "object" || kind == "null") =>
        {
            value["type"] = json!("object");
        }
        _ => return Err(Error::http(400, "Kiro 工具参数结构的根类型必须为object")),
    }
    if value.get("properties").is_none() && value.as_object().is_some_and(|map| map.len() == 1) {
        value["properties"] = json!({});
    }
    Ok(value)
}
pub fn arguments(value: &Value) -> Result<Value> {
    let value = if let Some(text) = value.as_str() {
        serde_json::from_str(text)
            .map_err(|_| Error::http(400, "工具历史参数不是有效JSON，未改写为空对象"))?
    } else {
        value.clone()
    };
    if !value.is_object() {
        return Err(Error::http(400, "Kiro 工具历史参数必须为JSON对象"));
    }
    Ok(value)
}
pub fn restore(body: &Value) -> Result<HashMap<String, String>> {
    let mut names = HashMap::new();
    let mut insert = |original: &str| -> Result<()> {
        if !util::identifier(original, 1024) {
            return Err(Error::http(400, "Kiro 工具名称为空或无效"));
        }
        let mapped = name(original);
        if let Some(previous) = names.insert(mapped, original.to_string()) {
            if previous != original {
                return Err(Error::http(400, "Kiro 工具规整名称发生冲突，请调整工具名"));
            }
        }
        Ok(())
    };
    for tool in body["tools"].as_array().into_iter().flatten() {
        insert(
            tool.pointer("/function/name")
                .or_else(|| tool.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(""),
        )?;
    }
    for message in body["messages"].as_array().into_iter().flatten() {
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            insert(call["function"]["name"].as_str().unwrap_or(""))?;
        }
    }
    Ok(names)
}
pub fn declarations(body: &Value) -> Result<Vec<Value>> {
    let list = body
        .get("tools")
        .filter(|list| !list.is_null())
        .map(|list| {
            list.as_array()
                .ok_or_else(|| Error::http(400, "Kiro 工具清单必须为数组"))
        })
        .transpose()?;
    let mut out = Vec::new();
    for tool in list.into_iter().flatten() {
        if tool
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind != "function")
        {
            return Err(Error::http(
                400,
                "Kiro 此入口仅接受函数工具；网页搜索请走独立搜索入口",
            ));
        }
        let function = tool.get("function").unwrap_or(tool);
        if function["strict"] == true || tool["strict"] == true {
            return Err(Error::http(
                400,
                "Kiro未提供工具参数strict保证，不能静默降为普通schema",
            ));
        }
        let original = function["name"]
            .as_str()
            .ok_or_else(|| Error::http(400, "Kiro 工具缺少名称"))?;
        let mapped = name(original);
        if out
            .iter()
            .any(|entry: &Value| entry["toolSpecification"]["name"] == mapped)
        {
            return Err(Error::http(400, "Kiro 工具清单包含重复名称"));
        }
        let description = function["description"]
            .as_str()
            .filter(|value| !value.is_empty())
            .unwrap_or(original);
        out.push(json!({ "toolSpecification": { "name": mapped, "description": description,
            "inputSchema": { "json": schema(function.get("parameters").or_else(|| function.get("input_schema")))? } } }));
    }
    if out.len() > 128 {
        return Err(Error::http(400, "Kiro 单次工具数量超过上限"));
    }
    Ok(out)
}
pub fn validate_choice(body: &Value) -> Result<()> {
    match body.get("tool_choice") {
        None | Some(Value::Null) => {}
        Some(Value::String(choice)) if choice == "auto" => {}
        Some(Value::String(choice))
            if choice == "none"
                && body["tools"].as_array().is_none_or(Vec::is_empty)
                && !body["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|message| {
                        message["tool_calls"]
                            .as_array()
                            .is_some_and(|calls| !calls.is_empty())
                    }) => {}
        _ => {
            return Err(Error::http(
                400,
                "Kiro 原生协议未提供此强制工具选择，不能静默忽略",
            ))
        }
    }
    if body["parallel_tool_calls"] == false {
        return Err(Error::http(400, "Kiro 未提供禁止并行工具的强制协议字段"));
    }
    Ok(())
}
