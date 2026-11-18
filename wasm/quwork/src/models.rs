//! 无鉴权公开目录；只接受已核对的 qurwork/OpenAI 固定域名。
//! compat 与 reasoningEfforts 一并进入宿主目录快照，重启后无需私有全局缓存。

use super::{client, endpoints};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use std::collections::HashSet;

const TTL: i64 = 600_000;
const LEGACY_KEY: &str = "quworkModelCatalog";

fn invalid() -> Error {
    Error::http(502, "QuWork 模型目录格式不兼容，已保留现有清单")
}

fn list(config: &Value, fetched: i64) -> Result<Value> {
    if config["key"] != "qurvis-desktop-model-config"
        || config["schemaVersion"] != 1
        || !config["enabled"].is_boolean()
    {
        return Err(invalid());
    }
    let providers = config["providers"]
        .as_array()
        .filter(|rows| rows.len() <= 8)
        .ok_or_else(invalid)?;
    let mut found = false;
    let mut ids = HashSet::new();
    let mut rows = Vec::new();
    for provider in providers
        .iter()
        .filter(|provider| provider["id"] == "qurwork")
    {
        found = true;
        let origin = provider["baseURL"]
            .as_str()
            .and_then(endpoints::model_origin);
        if provider["api"] != "openai-completions" || origin.is_none() {
            return Err(Error::http(
                502,
                "QuWork 模型协议或地址已变化，未向未确认的服务发送凭证",
            ));
        }
        if provider
            .get("enabled")
            .is_some_and(|value| !value.is_boolean())
        {
            return Err(invalid());
        }
        for row in provider["models"]
            .as_array()
            .filter(|rows| rows.len() <= 128)
            .ok_or_else(invalid)?
        {
            let id = row["id"]
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
                .ok_or_else(invalid)?;
            if !ids.insert(id.to_ascii_lowercase()) || !row["enabled"].is_boolean() {
                return Err(invalid());
            }
            if row["enabled"] == true && config["enabled"] == true && provider["enabled"] != false {
                rows.push((row, origin.ok_or_else(invalid)?));
            }
        }
    }
    if !found {
        return Err(invalid());
    }
    rows.sort_by_key(|(row, _)| row["sort"].as_i64().unwrap_or(i64::MAX));
    let default = config["defaultModel"]["modelId"].as_str().unwrap_or("");
    let models: Vec<Value> = rows
        .into_iter()
        .map(|(row, origin)| model(row, default, origin))
        .collect::<Result<_>>()?;
    Ok(json!({ "models": models, "meta": { "remote": fetched > 0, "refreshed_at": fetched } }))
}

fn model(row: &Value, default: &str, origin: &str) -> Result<Value> {
    let id = row["id"].as_str().ok_or_else(invalid)?;
    let mut value = json!({ "id": id, "name": row["name"].as_str().unwrap_or(id),
        "supportsToolCall": row["supportsToolCall"].as_bool().unwrap_or(true),
        "supportsImages": row["supportsImage"].as_bool().unwrap_or(false),
        "supportsReasoning": false, "isDefault": id == default, "quworkSchema": 1,
        "quworkModelOrigin": origin });
    for (target, source) in [
        ("maxInputTokens", "contextWindow"),
        ("maxOutputTokens", "maxTokens"),
    ] {
        if let Some(number) = row[source].as_u64().filter(|value| *value > 0) {
            value[target] = json!(number);
        }
    }
    let mut compat = json!({});
    for key in [
        "supportsStore",
        "supportsDeveloperRole",
        "supportsReasoningEffort",
        "requiresReasoningContentOnAssistantMessages",
        "supportsStrictMode",
    ] {
        if let Some(value) = row["compat"][key].as_bool() {
            compat[key] = json!(value);
        }
    }
    for key in ["maxTokensField", "thinkingFormat"] {
        if let Some(value) = row["compat"][key]
            .as_str()
            .filter(|value| value.len() <= 64 && !value.chars().any(char::is_control))
        {
            compat[key] = json!(value);
        }
    }
    value["compat"] = compat;
    if let Some(efforts) = row["reasoningEfforts"].as_object() {
        if efforts.len() > 16 {
            return Err(invalid());
        }
        let mut filtered = json!({});
        for (key, wire) in efforts {
            let wire = wire
                .as_str()
                .filter(|wire| {
                    !wire.is_empty() && wire.len() <= 32 && !wire.chars().any(char::is_control)
                })
                .ok_or_else(invalid)?;
            if key.is_empty() || key.len() > 32 || key.chars().any(char::is_control) {
                return Err(invalid());
            }
            filtered[key] = json!(wire);
        }
        value["supportsReasoning"] = json!(!efforts.is_empty());
        value["reasoningLevels"] = json!(efforts.keys().collect::<Vec<_>>());
        value["reasoningEffortLevels"] = value["reasoningLevels"].clone();
        value["reasoningEfforts"] = filtered;
    }
    Ok(value)
}

pub fn fallback() -> Result<Value> {
    let config: Value = serde_json::from_str(include_str!("models.json")).map_err(|_| invalid())?;
    list(&config, 0)
}

pub fn defaults() -> Vec<Value> {
    fallback()
        .ok()
        .and_then(|list| list["models"].as_array().cloned())
        .unwrap_or_default()
}

fn checked_rows(rows: &[Value]) -> bool {
    rows.iter().all(|row| {
        row["quworkSchema"] == 1
            && row["quworkModelOrigin"]
                .as_str()
                .and_then(endpoints::model_origin)
                .is_some()
    })
}

pub fn cached() -> Result<Value> {
    let cached = sdk::catalog::cached()?;
    if cached["models"].is_array() && cached["meta"].is_object() {
        // 真实空目录是禁用事实，不能回落成旧可用模型。
        if cached["meta"]["remote"] == true
            && cached["models"].as_array().is_some_and(Vec::is_empty)
        {
            return Ok(cached);
        }
        if cached["models"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty() && checked_rows(rows))
        {
            return Ok(cached);
        }
    }
    // 旧原生缓存保存的是完整config；仅在它不旧于宿主快照时恢复兼容规则。
    if let Ok(saved) = sdk::host("settings.get", json!({ "key": LEGACY_KEY })) {
        let at = saved["fetchedAt"].as_i64().unwrap_or(0);
        let current_at = cached["meta"]["refreshed_at"].as_i64().unwrap_or(0);
        if cached["meta"]["remote"] != true || at >= current_at {
            if let Ok(list) = list(&saved["config"], at) {
                return Ok(list);
            }
        }
    }
    if cached["meta"]["remote"] == true && cached["models"].is_array() {
        // 有远程快照但不含必要的兼容规则时保留其模型集合；对话要求先重新刷新。
        return Ok(cached);
    }
    fallback()
}

pub fn refresh(force: bool) -> Result<Value> {
    let mut previous = cached()?;
    if let Some(fields) = previous.as_object_mut() {
        fields.remove("message");
    }
    previous["refreshed"] = json!(false);
    previous["count"] = json!(previous["models"].as_array().map(Vec::len).unwrap_or(0));
    let age = sdk::now().saturating_sub(previous["meta"]["refreshed_at"].as_i64().unwrap_or(0));
    if !force
        && previous["meta"]["remote"] == true
        && (0..TTL).contains(&age)
        && previous["models"]
            .as_array()
            .is_some_and(|rows| checked_rows(rows))
    {
        return Ok(previous);
    }
    match client::public_config().and_then(|config| list(&config, sdk::now())) {
        Ok(mut list) => {
            list["refreshed"] = json!(true);
            list["count"] = json!(list["models"].as_array().map(Vec::len).unwrap_or(0));
            Ok(list)
        }
        Err(error) => {
            previous["message"] = json!(error.message);
            Ok(previous)
        }
    }
}

pub fn find(requested: &str) -> Result<Value> {
    let list = cached()?;
    let row = list["models"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| {
            row["id"].as_str() == Some(requested) || row["name"].as_str() == Some(requested)
        })
        .cloned()
        .ok_or_else(|| {
            Error::http(400, "QuWork 当前目录没有这个模型，请先刷新模型列表")
                .with_code("local_precondition")
        })?;
    if row["quworkSchema"] != 1 {
        return Err(
            Error::http(400, "QuWork 目录缺少兼容规则，请先刷新模型列表")
                .with_code("local_precondition"),
        );
    }
    // 旧WASM清单没有来源字段时沿用原固定域名，不把升级变成必须手工刷新。
    let origin = match row.get("quworkModelOrigin") {
        Some(value) => value.as_str().and_then(endpoints::model_origin),
        None => Some(endpoints::LEGACY_MODEL),
    }
    .ok_or_else(|| {
        Error::http(400, "QuWork 模型来源尚未确认，请先刷新模型列表")
            .with_code("local_precondition")
    })?;
    let mut row = row;
    row["quworkModelOrigin"] = json!(origin);
    Ok(row)
}
