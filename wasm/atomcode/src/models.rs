use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

use super::{account, upstream};

pub fn cached() -> Result<Value> {
    let list = sdk::catalog::cached()?;
    Ok(if list.is_object() { list } else { json!({ "models": [], "meta": { "remote": false, "refreshed_at": 0 } }) })
}

pub fn tier(status: &Value) -> Option<&'static str> {
    let name = super::usage::plan(status)["plan_name"].as_str()
        .or_else(|| status["plan_type"].as_str()).unwrap_or("").to_ascii_lowercase();
    if name.contains("max") { Some("Max") } else if name.contains("pro") { Some("Pro") }
    else if name.contains("lite") { Some("Lite") } else { None }
}

fn model(item: &Value) -> Option<Value> {
    if item["plan_available"] != true || item["status"] == "deprecated" { return None; }
    let id = item["display_model_name"].as_str()?.trim();
    if id.is_empty() { return None; }
    let name = match id {
        "glm5.3-flash" => "GLM-5.3-Flash",
        "qwen3.8-27b" => "Qwen3.8-27B",
        "deepseek-flash" => "DeepSeek-V4.1-Flash",
        _ => id,
    };
    let levels: Vec<&str> = item["reasoning_effort_levels"].as_array().into_iter().flatten()
        .filter_map(Value::as_str).map(str::trim).filter(|level| !level.is_empty()).collect();
    let mut value = json!({
        "id": id, "name": name, "supportsToolCall": true,
        "supportsImages": item["supports_vision"].as_bool().unwrap_or(false),
        "supportsVideo": false, "supportsReasoning": !levels.is_empty(),
        "reasoningEffortLevels": levels, "reasoningLevels": levels
    });
    if let Some(default) = item["default_reasoning_effort"].as_str().filter(|value| levels.contains(value)) {
        value["reasoningDefaultLevel"] = json!(default);
    }
    if let Some(context) = item["context_window"].as_i64().filter(|n| *n > 0) { value["maxInputTokens"] = json!(context); }
    Some(value)
}

pub fn fetch(token: &str) -> Result<Value> {
    let status = upstream::status(token)?;
    let tier = tier(&status).ok_or_else(|| Error::new("AtomCode 套餐身份尚未确认，已保留当前账号目录"))?;
    let payload = upstream::get(&format!("/coding-plan/models-v2?plan_type={tier}"), token)?;
    let entries = payload["models"].as_array().or_else(|| payload.as_array())
        .ok_or_else(|| Error::new("AtomCode 模型目录缺少模型数组"))?;
    if entries.iter().any(|item| item["status"] != "deprecated" &&
        (!item["plan_available"].is_boolean() || item["plan_available"] == true
            && item["display_model_name"].as_str().is_none_or(|id| id.trim().is_empty()))) {
        return Err(Error::new("AtomCode 模型目录的权益或名称字段无效，已保留最后成功目录"));
    }
    let models: Vec<Value> = entries.iter().filter_map(model).collect();
    Ok(json!({ "models": models, "meta": { "remote": true, "refreshed_at": sdk::now() } }))
}

pub fn refresh(force: bool) -> Result<Value> {
    if !force {
        let previous = cached()?;
        let changed = claim_changed_at()?;
        let refreshed_at = previous["meta"]["refreshed_at"].as_i64().unwrap_or(0);
        let age = sdk::now() - refreshed_at;
        if changed <= refreshed_at && previous["meta"]["remote"] == true && previous["models"].is_array() && (0..7_200_000).contains(&age) {
            return Ok(json!({ "refreshed": false, "count": previous["models"].as_array().map(Vec::len).unwrap_or(0) }));
        }
    }
    let token = account::ensure()?;
    let mut list = fetch(&token)?;
    list["refreshed"] = json!(true);
    list["count"] = json!(list["models"].as_array().map(Vec::len).unwrap_or(0));
    Ok(list)
}

fn claim_changed_at() -> Result<i64> {
    let state = sdk::state::get(sdk::state::Scope::Account, "claim", 1)?;
    if !state.compatible { return Err(Error::new("套餐领取记忆格式不兼容，未使用其他账号的模型目录")); }
    let outcome = state.value["result"]["outcome"].as_str().unwrap_or("");
    Ok(if matches!(outcome, "claimed" | "done") { state.value["at"].as_i64().unwrap_or(0) } else { 0 })
}

pub fn wire_id(requested: &str, account_id: &str) -> Result<String> {
    let prefix = format!("{account_id}/");
    let name = requested.strip_prefix(&prefix).unwrap_or(requested).trim();
    let mut list = cached()?;
    let dirty = claim_changed_at()?;
    if dirty > list["meta"]["refreshed_at"].as_i64().unwrap_or(0) {
        list = fetch(&account::ensure()?)?;
        if !list["models"].as_array().into_iter().flatten().any(|entry| entry["id"] == name || entry["name"] == name) {
            return Err(Error::http(403, "AtomCode 套餐已变化，当前账号的新目录未包含所选模型，请刷新模型列表"));
        }
    }
    Ok(list["models"].as_array().into_iter().flatten().find(|model| model["id"] == name || model["name"] == name)
        .and_then(|model| model["id"].as_str()).unwrap_or(name).to_string())
}
