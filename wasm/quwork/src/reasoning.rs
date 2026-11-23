use super::models;
use agent2api_wasm_sdk::{json, Result, Value};

pub fn client_level(body: &Value) -> Option<String> {
    let normalize = |value: &str| {
        let value = value.trim();
        (!value.is_empty() && value.chars().count() <= 32).then(|| value.to_string())
    };
    for key in [
        "reasoning_effort",
        "reasoningEffort",
        "effort",
        "reasoning",
        "thinking",
    ] {
        if let Some(value) = body[key].as_str() {
            return normalize(value);
        }
        if key == "reasoning" {
            if let Some(value) = body[key]["effort"].as_str().and_then(normalize) {
                return Some(value);
            }
        }
    }
    None
}

pub fn patch(level: &str, model: &str, body: &Value) -> Result<Value> {
    let skip =
        || json!({ "action": "skip", "reason": "QuWork 按当前模型目录与客户端设置处理思考档位" });
    if client_level(body).is_some() || body.get("thinking").is_some_and(|value| !value.is_null()) {
        return Ok(skip());
    }
    let row = match models::find(model) {
        Ok(row) => row,
        Err(_) => return Ok(skip()),
    };
    let level = if level == "none" { "off" } else { level };
    let Some(wire) = row["reasoningEfforts"][level].as_str() else {
        return Ok(skip());
    };
    Ok(json!({ "action": "set", "field": "reasoning_effort", "value": wire }))
}

pub fn apply(body: &mut Value, row: &Value, mapped: &str) {
    if row["compat"]["thinkingFormat"] != "deepseek"
        || body.get("thinking").is_some_and(|value| !value.is_null())
    {
        return;
    }
    let explicit = client_level(body);
    let level = explicit.as_deref().unwrap_or(mapped);
    let level = if level == "none" { "off" } else { level };
    let Some(wire) = row["reasoningEfforts"][level].as_str() else {
        return;
    };
    if level == "off" {
        body["thinking"] = json!({ "type": "disabled" });
        if let Some(object) = body.as_object_mut() {
            object.remove("reasoning_effort");
        }
    } else {
        body["thinking"] = json!({ "type": "enabled" });
        body["reasoning_effort"] = json!(wire);
    }
}
