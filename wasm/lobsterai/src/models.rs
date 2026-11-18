//! 账号独立目录供宿主持久化与聚合；展示清单不替代上游对点名模型的资格判断。
//! 刷新失败保留旧快照，成功空目录则按真实结果保存。

use super::{account, reasoning, upstream, version};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

const TTL: i64 = 2 * 60 * 60 * 1000;
const FALLBACK: &[&str] = &[
    "deepseek-v4-flash",
    "deepseek-v4-pro",
    "MiniMax-M3",
    "MiniMax-M2.7",
    "qwen3.7-max",
    "qwen3.7-plus",
    "qwen3.6-plus",
    "qwen3.5-plus-2026-04-20",
    "kimi-k2.7-code",
    "kimi-k2.7-code-highspeed",
    "kimi-k2.6",
    "kimi-k2.5",
    "doubao-seed-2-1-pro-260628",
    "doubao-seed-2-1-turbo-260628",
    "doubao-seed-2-0-code-preview-260215",
    "glm-5.2",
    "glm-5.1",
    "glm-5v-turbo",
    "glm-5",
];

pub fn defaults() -> Vec<Value> {
    // 沿用旧静态表的桥接估计窗口，并非逐模型实测；在线结果一旦可用即取代它。
    FALLBACK
        .iter()
        .map(|id| {
            json!({ "id": id, "name": id, "maxInputTokens": 131_072,
        "supportsToolCall": true, "supportsImages": false, "supportsVideo": false,
        "supportsReasoning": false })
        })
        .collect()
}

pub fn cached() -> Result<Value> {
    let value = sdk::catalog::cached()?;
    if value["models"].is_array() && value["meta"]["remote"] == true {
        return Ok(value);
    }
    Ok(json!({ "models": defaults(), "meta": { "remote": false, "refreshed_at": 0 } }))
}

fn thinking(item: &Value) -> bool {
    item["supportsThinking"] == true
        || item["thinkingConfig"]["options"]
            .as_array()
            .is_some_and(|options| {
                options
                    .iter()
                    .filter(|option| {
                        option["openclawLevel"]
                            .as_str()
                            .is_some_and(|level| !level.trim().is_empty())
                    })
                    .count()
                    > 1
            })
}

fn model(item: &Value) -> Option<Value> {
    if item["accessible"] == false || item["busy"] == true {
        return None;
    }
    let id = item["modelId"].as_str()?.trim();
    if id.is_empty() || id.len() > 512 || id.chars().any(char::is_control) {
        return None;
    }
    let name = item["modelName"]
        .as_str()
        .map(str::trim)
        .filter(|name| {
            !name.is_empty() && name.len() <= 1024 && !name.chars().any(char::is_control)
        })
        .unwrap_or(id);
    let mut result = json!({ "id": id, "name": name,
        "supportsToolCall": item["supportsToolCalling"].as_bool().unwrap_or(true),
        "supportsImages": item["supportsImage"].as_bool().unwrap_or(false), "supportsVideo": false,
        "supportsReasoning": thinking(item) });
    if let Some(profile) = reasoning::profile(&item["thinkingConfig"]) {
        result["thinkingProfile"] = profile;
    }
    let levels = if result["supportsReasoning"] == true {
        reasoning::levels(id, &result["thinkingProfile"])
    } else {
        Vec::new()
    };
    if !levels.is_empty() {
        result["reasoningEffortLevels"] = json!(levels);
        result["reasoningLevels"] = result["reasoningEffortLevels"].clone();
        let default = reasoning::default_level(id, &result["thinkingProfile"]);
        if !default.is_empty() {
            result["reasoningEffortDefault"] = json!(default);
        }
    }
    for (source, target) in [
        ("contextWindow", "maxInputTokens"),
        ("maxTokens", "maxOutputTokens"),
    ] {
        if let Some(value) = account::number(&item[source]).filter(|value| *value > 0.0) {
            result[target] = json!(value as i64);
        }
    }
    // 0 倍率表示免费，不是缺字段。
    if let Some(multiplier) = account::number(&item["costMultiplier"]) {
        result["credits"] = json!(if multiplier == 0.0 {
            "免费".to_string()
        } else {
            format!("x{multiplier}")
        });
    }
    Some(result)
}

pub fn fetch() -> Result<Value> {
    let record = account::prepared()?;
    let token = account::access(&record["secret"])?;
    let version = version::resolve();
    account::validate_current(&record)?;
    let query = account::keyfrom_fields(&record["secret"], &version)
        .into_iter()
        .map(|(key, value)| format!("{key}={}", upstream::query(&value)))
        .collect::<Vec<_>>()
        .join("&");
    // query 故意不含 refreshToken；上游明确要求的载荷与 Authorization 分开。
    let value = upstream::request(
        "GET",
        &format!("/api/models/available?{query}"),
        Some(&token),
        Some(&version),
        None,
    )?;
    let data = upstream::envelope(&value, "模型目录")?;
    account::validate_current(&record)?;
    let entries = data
        .as_array()
        .or_else(|| data["data"].as_array())
        .ok_or_else(|| Error::http(502, "LobsterAI 目录响应缺少模型数组"))?;
    if entries.len() > 4096 {
        return Err(Error::http(502, "LobsterAI 目录条目超过上限"));
    }
    let mut models: Vec<Value> = Vec::new();
    for entry in entries {
        if !entry.is_object()
            || entry["modelId"].as_str().is_none_or(|id| {
                id.trim().is_empty() || id.len() > 512 || id.chars().any(char::is_control)
            })
        {
            return Err(Error::http(
                502,
                "LobsterAI 模型目录条目格式无效，已保留原清单",
            ));
        }
        if let Some(model) = model(entry) {
            if !models.iter().any(|row| row["id"] == model["id"]) {
                models.push(model);
            }
        }
    }
    if json!(&models).to_string().len() > 240 * 1024 {
        return Err(Error::http(
            502,
            "LobsterAI 模型目录超过保存限制，已保留原清单",
        ));
    }
    Ok(
        json!({ "refreshed": true, "count": models.len(), "models": models,
        "meta": { "remote": true, "refreshed_at": sdk::now() } }),
    )
}

pub fn refresh(params: &Value) -> Result<Value> {
    let mut previous = match cached() {
        Ok(value) => value,
        Err(error) => return Ok(sdk::catalog::failed(&error.message)),
    };
    let count = previous["models"].as_array().map(Vec::len).unwrap_or(0);
    previous["refreshed"] = json!(false);
    previous["count"] = json!(count);
    if !params["force"].as_bool().unwrap_or(false) && previous["meta"]["remote"] == true {
        let age = sdk::now().saturating_sub(previous["meta"]["refreshed_at"].as_i64().unwrap_or(0));
        if (0..TTL).contains(&age) {
            return Ok(previous);
        }
    }
    if params["account"].as_str().is_none_or(str::is_empty) {
        previous["message"] = json!("LobsterAI 在线目录需要主程序授予一个账号，请选择账号刷新");
        return Ok(previous);
    }
    match fetch() {
        Ok(value) => Ok(value),
        Err(error) => {
            previous["message"] = json!(error.message);
            Ok(previous)
        }
    }
}

pub fn for_request(requested: &str) -> Result<Value> {
    let list = cached()?;
    if list["meta"]["remote"] == true {
        return Ok(list["models"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|item| item["id"] == requested)
            .cloned()
            .unwrap_or(Value::Null));
    }
    let fetched = match fetch() {
        Ok(value) => value,
        Err(error)
            if matches!(
                error.code.as_deref(),
                Some(
                    "lobsterai_session"
                        | "aborted"
                        | "account_identity_changed"
                        | "account_credentials_changed"
                )
            ) =>
        {
            return Err(error)
        }
        Err(_) => return Ok(Value::Null),
    };
    Ok(fetched["models"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| item["id"] == requested)
        .cloned()
        .unwrap_or(Value::Null))
}

pub fn wire_id(requested: &str) -> Result<String> {
    let target = requested.trim();
    let list = cached()?;
    if list["meta"]["remote"] == true {
        if let Some(id) = list["models"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|item| item["id"] == target || item["name"] == target)
            .and_then(|item| item["id"].as_str())
        {
            return Ok(id.to_string());
        }
    }
    for (alias, wire) in [
        ("deepseek-v4.1-flash", "deepseek-flash"),
        ("glm-5.3-flash", "glm-5.3-flash"),
        ("glm-5.3", "glm-5.3"),
        ("qwen3.8-max", "qwen3.8-max"),
        ("qwen3.8-flash-next", "qwen3.8-flash"),
        ("kimi-k2.7-code", "kimi-k2.7-code"),
    ] {
        if target.eq_ignore_ascii_case(alias) {
            return Ok(wire.to_string());
        }
    }
    Ok(target.to_string())
}
