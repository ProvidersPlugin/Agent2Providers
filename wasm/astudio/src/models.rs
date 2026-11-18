//! 原生目录按账号鉴权，10分钟刷新窗口。catalog.accounts.v1由主机隔离/持久化，不能遍历其它账号。
//! 成功目录中仅投影真实能力字段；兜底表不臆测图片能力。失败保留上一份成功目录。

use super::{account, client};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{Scope, Snapshot},
    Error, Result, Value,
};
use sha2::Digest;
use std::collections::HashSet;

const TTL: i64 = 600_000;
const KEY: &str = "catalog-credential";

pub fn defaults() -> Vec<Value> {
    [
        ("astronclaw-auto", "Auto", 262_144, 32_768, false),
        ("spark-x2.6", "Spark-X2.6", 262_144, 32_768, false),
        ("spark-x2.6-4b", "Spark-X2.6-Lite", 1_048_576, 110_000, true),
        ("spark-x2.5", "Spark-X2.5", 262_144, 32_768, false),
        ("xopglm52", "GLM-5.2", 1_048_576, 32_768, false),
        ("xopdeepseekv4pro0813", "DeepSeek-V4-Pro", 1_048_576, 32_768, false),
        ("xopdsv4flash0731in", "DeepSeek-V4-Flash", 1_048_576, 32_768, false),
    ].into_iter().map(|(id, name, context, output, free)| {
        let mut model = json!({ "id": id, "name": name, "maxInputTokens": context, "maxOutputTokens": output,
            "supportsToolCall": true, "supportsReasoning": true, "isDefault": id == "astronclaw-auto", "kind": "chat" });
        if free { model["free"] = json!(true); }
        model
    }).collect()
}

fn fallback() -> Value {
    json!({ "models": defaults(), "meta": { "remote": false, "refreshed_at": 0 } })
}

pub fn cached() -> Result<Value> {
    let list = sdk::catalog::cached()?;
    Ok(if list["models"].is_array() && list["meta"].is_object() {
        list
    } else {
        fallback()
    })
}

pub fn list(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        return Ok(fallback());
    }
    cached()
}

fn parse(data: &Value) -> Result<Vec<Value>> {
    let rows = data["models"]
        .as_array()
        .filter(|rows| rows.len() <= 4096)
        .ok_or_else(|| Error::http(502, "AStudio 模型目录格式异常"))?;
    let mut models = Vec::new();
    let mut seen = HashSet::new();
    for row in rows {
        if row["visibility"] == "hidden" || row["supported_in_api"] == false {
            continue;
        }
        let Some(id) = row["slug"]
            .as_str()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            return Err(Error::http(
                502,
                "AStudio 模型目录包含无效条目，保留现有清单",
            ));
        };
        if id.len() > 512 || id.chars().any(char::is_control) {
            return Err(Error::http(502, "AStudio 模型标识格式异常"));
        }
        if !seen.insert(id.to_string()) {
            continue;
        }
        let name = account::text(row, &["display_name"]);
        let context = row["context_window"].as_i64().filter(|value| *value > 0);
        let output = row["max_output_tokens"]
            .as_i64()
            .filter(|value| *value > 0)
            .unwrap_or(32_768);
        let levels: Vec<Value> = row["supported_reasoning_levels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| row["effort"].as_str())
            .filter(|level| !level.is_empty())
            .map(|level| json!(level))
            .collect();
        let mut model = json!({ "id": id, "name": if name.is_empty() { id } else { &name },
            "maxOutputTokens": output, "supportsToolCall": true, "kind": "chat",
            "supportsReasoning": levels.iter().any(|level| *level != "none"), "isDefault": id == "astronclaw-auto" });
        if let Some(context) = context {
            model["maxInputTokens"] = json!(context);
        }
        if let Some(modalities) = row["input_modalities"].as_array() {
            model["supportsImages"] = json!(modalities.iter().any(|value| *value == "image"));
        }
        if !levels.is_empty() {
            model["reasoningEffortLevels"] = json!(levels);
        }
        if row
            .pointer("/badge/text/en")
            .is_some_and(|value| *value == "Free")
        {
            model["free"] = json!(true);
        }
        models.push(model);
    }
    models.sort_by_key(|model| model["isDefault"] != true);
    if serde_json::to_vec(&models)
        .map_err(|_| Error::http(502, "AStudio 目录无法序列化"))?
        .len()
        > 240 * 1024
    {
        return Err(Error::http(502, "AStudio 模型目录超过受控缓存大小"));
    }
    Ok(models)
}

pub fn fetch(key: &str) -> Result<Vec<Value>> {
    parse(&client::catalog(key)?.data(false, "model")?)
}

fn outcome(mut value: Value, refreshed: bool, message: Option<String>) -> Value {
    value["refreshed"] = json!(refreshed);
    value["count"] = json!(value["models"].as_array().map(Vec::len).unwrap_or(0));
    if let Some(message) = message {
        value["message"] = json!(message);
    }
    value
}

fn state() -> Result<Snapshot> {
    let state = sdk::state::get(Scope::Account, KEY, 1)?;
    if !state.compatible {
        return Err(Error::http(409, "AStudio 目录状态版本不兼容"));
    }
    Ok(state)
}

pub fn authorize(secret: &Value, model: &str) -> Result<()> {
    let fingerprint = format!(
        "{:x}",
        sha2::Sha256::digest(account::catalog_key(secret)?.as_bytes())
    );
    let snapshot = state()?;
    let cached = cached()?;
    let at = cached["meta"]["refreshed_at"].as_i64().unwrap_or(0);
    let models = if cached["meta"]["remote"] == true
        && snapshot.value["fingerprint"] == fingerprint
        && snapshot.value["at"].as_i64() == Some(at)
        && (0..TTL).contains(&sdk::now().saturating_sub(at))
    {
        cached["models"].as_array().cloned().unwrap_or_default()
    } else {
        // 账号凭据更换后的旧缓存不能授权当前模型，当前调用现场重取，不问其它账号。
        fetch(&account::catalog_key(secret)?)?
    };
    if !models.iter().any(|row| {
        row["id"]
            .as_str()
            .is_some_and(|id| id.eq_ignore_ascii_case(model))
    }) {
        return Err(Error::http(
            403,
            "AStudio 当前账号目录中没有该模型，请刷新目录或检查映射",
        )
        .with_code("local_precondition"));
    }
    Ok(())
}

pub fn refresh(params: &Value) -> Result<Value> {
    let record = account::current()?;
    let previous = cached()?;
    let fingerprint = format!(
        "{:x}",
        sha2::Sha256::digest(account::catalog_key(&record["secret"])?.as_bytes())
    );
    let state = state()?;
    let now = sdk::now();
    let at = previous["meta"]["refreshed_at"].as_i64().unwrap_or(0);
    if params["force"] != true
        && previous["meta"]["remote"] == true
        && (0..TTL).contains(&now.saturating_sub(at))
        && state.value["fingerprint"] == fingerprint
        && state.value["at"].as_i64() == Some(at)
    {
        return Ok(outcome(previous, false, None));
    }
    let models = match fetch(&account::catalog_key(&record["secret"])?) {
        Ok(models) => models,
        Err(error) if error.code.as_deref() == Some("astudio_model_expired") => return Err(error),
        Err(error) => return Ok(outcome(previous, false, Some(error.message))),
    };
    account::ensure_current(&record)?;
    if !sdk::state::compare_exchange(
        Scope::Account,
        KEY,
        1,
        state.revision,
        json!({ "fingerprint": fingerprint, "at": now }),
        0,
    )? {
        return Err(Error::http(409, "AStudio 目录已被另一轮刷新，请重试"));
    }
    Ok(
        json!({ "refreshed": true, "count": models.len(), "models": models,
        "meta": { "remote": true, "refreshed_at": now } }),
    )
}
