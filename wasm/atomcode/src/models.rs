use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

use super::{account, upstream};

pub fn cached() -> Result<Value> {
    let list = sdk::catalog::cached()?;
    Ok(if list.is_object() { list } else { json!({ "models": [], "meta": { "remote": false, "refreshed_at": 0 } }) })
}

pub fn tier(status: &Value) -> Option<&'static str> {
    let name = status["codingplan_free"]["plan_name"].as_str()
        .or_else(|| status["plan_type"].as_str()).unwrap_or("").to_ascii_lowercase();
    if name.contains("max") { Some("Max") } else if name.contains("pro") { Some("Pro") }
    else if name.contains("lite") { Some("Lite") } else { None }
}

fn model(item: &Value) -> Option<Value> {
    if item["plan_available"] != true { return None; }
    let id = item["display_model_name"].as_str()?.trim();
    if id.is_empty() { return None; }
    let name = match id {
        "glm5.3-flash" => "GLM-5.3-Flash",
        "qwen3.8-27b" => "Qwen3.8-27B",
        "deepseek-flash" => "DeepSeek-V4.1-Flash",
        _ => id,
    };
    let levels: Vec<&str> = item["reasoning_effort_levels"].as_array().into_iter().flatten()
        .filter_map(Value::as_str).collect();
    let mut value = json!({
        "id": id, "name": name, "supportsToolCall": true,
        "supportsImages": item["supports_vision"].as_bool().unwrap_or(false),
        "supportsVideo": false, "supportsReasoning": !levels.is_empty(),
        "reasoningEffortLevels": levels
    });
    if let Some(context) = item["context_window"].as_i64().filter(|n| *n > 0) { value["maxInputTokens"] = json!(context); }
    Some(value)
}

pub fn fetch(token: &str) -> Result<Value> {
    let status = upstream::status(token)?;
    let tier = tier(&status).ok_or_else(|| Error::new("AtomCode 套餐身份尚未确认，已保留当前账号目录"))?;
    let payload = upstream::get(&format!("/coding-plan/models-v2?plan_type={tier}"), token)?;
    let entries = payload["models"].as_array().or_else(|| payload.as_array())
        .ok_or_else(|| Error::new("AtomCode 模型目录缺少模型数组"))?;
    let models: Vec<Value> = entries.iter().filter_map(model).collect();
    Ok(json!({ "models": models, "meta": { "remote": true, "refreshed_at": sdk::now() } }))
}

pub fn refresh(force: bool) -> Result<Value> {
    if !force {
        let previous = cached()?;
        let age = sdk::now() - previous["meta"]["refreshed_at"].as_i64().unwrap_or(0);
        if previous["meta"]["remote"] == true && previous["models"].is_array() && (0..7_200_000).contains(&age) {
            return Ok(json!({ "refreshed": false, "count": previous["models"].as_array().map(Vec::len).unwrap_or(0) }));
        }
    }
    let token = account::ensure()?;
    let mut list = fetch(&token)?;
    list["refreshed"] = json!(true);
    list["count"] = json!(list["models"].as_array().map(Vec::len).unwrap_or(0));
    Ok(list)
}

pub fn wire_id(requested: &str, account_id: &str) -> Result<String> {
    let prefix = format!("{account_id}/");
    let name = requested.strip_prefix(&prefix).unwrap_or(requested).trim();
    let list = cached()?;
    Ok(list["models"].as_array().into_iter().flatten().find(|model| model["id"] == name || model["name"] == name)
        .and_then(|model| model["id"].as_str()).unwrap_or(name).to_string())
}
