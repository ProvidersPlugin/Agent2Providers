//! 仅消费官方目录中已核对的 JSON Pointer 思考字段，不让目录规则改消息、工具或鉴权。
use agent2api_wasm_sdk::{json, Error, Result, Value};

const PATHS: &[&str] = &[
    "/thinking",
    "/thinking/type",
    "/reasoning_effort",
    "/output_config/effort",
];

pub fn apply(body: &mut Value, model: &Value, level: &str) -> Result<()> {
    if level.is_empty() {
        return Ok(());
    }
    let Some(rule) = model["reasoningRules"]["levels"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|rule| rule["id"] == level)
    else {
        return Err(Error::http(400, "该思考档位不在科搭当前模型的官方目录内"));
    };
    if let Some(omit) = rule["omit"].as_array() {
        for path in omit.iter().filter_map(Value::as_str) {
            remove(body, path)?;
        }
    }
    if let Some(fields) = rule["set"].as_object() {
        for (path, value) in fields {
            set(body, path, value.clone())?;
        }
    }
    Ok(())
}

fn checked(path: &str) -> Result<Vec<&str>> {
    if !PATHS.contains(&path) {
        return Err(Error::http(502, "科搭思考规则包含未授权的请求字段"));
    }
    Ok(path.trim_start_matches('/').split('/').collect())
}

fn remove(body: &mut Value, path: &str) -> Result<()> {
    let keys = checked(path)?;
    let target = if keys.len() == 1 {
        body
    } else {
        let Some(target) = body.get_mut(keys[0]) else {
            return Ok(());
        };
        target
    };
    if let Some(fields) = target.as_object_mut() {
        fields.remove(keys[keys.len() - 1]);
    }
    Ok(())
}

fn set(body: &mut Value, path: &str, value: Value) -> Result<()> {
    let keys = checked(path)?;
    if keys.len() == 1 {
        body[keys[0]] = value;
        return Ok(());
    }
    if body[keys[0]].is_null() {
        body[keys[0]] = json!({});
    }
    if !body[keys[0]].is_object() {
        return Err(Error::http(400, "科搭思考参数格式无效"));
    }
    body[keys[0]][keys[1]] = value;
    Ok(())
}
