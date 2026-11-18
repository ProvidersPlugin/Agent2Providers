//! session 拉取的真实目录按账号持久化，无内置模型。保留 protocol、倍率及能力位映射。
//! 实际目录按账号持久化，自动缓存10分钟，force绕过；合法空目录与读取失败分开。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

use super::{account, client, errors};

const TTL_MS: i64 = 600_000;

fn empty() -> Value {
    json!({ "models": [], "meta": { "remote": false, "refreshed_at": 0 } })
}

pub fn cached() -> Result<Value> {
    let value = sdk::catalog::cached()?;
    Ok(if value["models"].is_array() && value["meta"].is_object() {
        value
    } else {
        empty()
    })
}

pub fn list(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        Ok(empty())
    } else {
        cached()
    }
}

fn outcome(mut list: Value, refreshed: bool, message: Option<String>) -> Value {
    list["refreshed"] = json!(refreshed);
    list["count"] = json!(list["models"].as_array().map(Vec::len).unwrap_or(0));
    if let Some(message) = message {
        list["message"] = json!(message);
    }
    list
}

pub fn fetch(session: &str) -> Result<Value> {
    let payload = client::token_request("GET", "/api/v1/models", session, None, "模型目录查询")?;
    let code = errors::business_code(&payload);
    if !code.is_empty() {
        errors::ensure_success(&payload, "模型目录查询", session)?;
    }
    let rows = payload
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| payload.as_array())
        .ok_or_else(|| Error::http(502, "Loomy 模型目录格式无效（保留现有清单）"))?;
    let mut models = Vec::with_capacity(rows.len());
    let mut ids = std::collections::BTreeSet::new();
    for row in rows {
        let item =
            map_entry(row).ok_or_else(|| Error::http(502, "Loomy 模型条目无效（保留现有清单）"))?;
        let id = item["id"].as_str().unwrap_or("");
        if id.len() > 512
            || id.chars().any(char::is_control)
            || !ids.insert(id.to_ascii_lowercase())
        {
            return Err(Error::http(502, "Loomy 模型标识无效或重复（保留现有清单）"));
        }
        models.push(item);
    }
    Ok(sdk::catalog::refreshed(models))
}

pub fn refresh(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        return Ok(outcome(empty(), false, None));
    }
    let record = account::current()?;
    let previous = cached()?;
    let age = sdk::now().saturating_sub(previous["meta"]["refreshed_at"].as_i64().unwrap_or(0));
    if params["force"] != true
        && previous["models"].is_array()
        && previous["meta"]["remote"] == true
        && (0..TTL_MS).contains(&age)
    {
        account::ensure_current(&record)?;
        return Ok(outcome(previous, false, None));
    }
    let result = account::token(&record["secret"]).and_then(|session| fetch(&session));
    account::ensure_current(&record)?;
    match result {
        Ok(value) => Ok(value),
        Err(error) => Ok(outcome(previous, false, Some(error.message))),
    }
}

pub fn entry(model: &str, session: &str) -> Result<Option<Value>> {
    let lookup = |list: &Value| {
        list["models"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|entry| {
                entry["id"]
                    .as_str()
                    .is_some_and(|id| id.eq_ignore_ascii_case(model.trim()))
            })
            .cloned()
    };
    if let Some(value) = lookup(&cached()?) {
        return Ok(Some(value));
    }
    // 通用宿主一般已取得账号目录；缺项时在本次调用内拉取实际协议，不猜模型名。
    Ok(lookup(&fetch(session)?))
}

pub fn protocol(model: &str, session: &str) -> Result<String> {
    let entry = entry(model, session)?;
    if entry.as_ref().is_some_and(|entry| entry["kind"] == "image") {
        return Err(Error::http(400, "Loomy 图片模型请使用图片生成或编辑入口"));
    }
    Ok(entry
        .and_then(|entry| entry["protocol"].as_str().map(str::to_string))
        .unwrap_or_else(|| "openai".to_string()))
}

fn normalize_protocol(protocol: Option<&str>) -> &'static str {
    match protocol
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("anthropic" | "anthropic_messages") => "anthropic",
        _ => "openai",
    }
}

fn split_rate(name: &str) -> (String, Option<String>) {
    let trimmed = name.trim_end();
    let (body, inner) = if let Some(body) = trimmed.strip_suffix(')') {
        match body.rfind('(') {
            Some(open) => (&body[..open], body[open + 1..].trim()),
            None => return (name.into(), None),
        }
    } else if let Some(body) = trimmed.strip_suffix('）') {
        match body.rfind('（') {
            Some(open) => (&body[..open], body[open + '（'.len_utf8()..].trim()),
            None => return (name.into(), None),
        }
    } else {
        return (name.into(), None);
    };
    let Some(number) = inner.strip_prefix('x').or_else(|| inner.strip_prefix('X')) else {
        return (name.into(), None);
    };
    let number = number.trim();
    if number.is_empty()
        || !number.chars().any(|ch| ch.is_ascii_digit())
        || !number.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
    {
        return (name.into(), None);
    }
    (
        if body.trim_end().is_empty() {
            name.into()
        } else {
            body.trim_end().to_string()
        },
        Some(format!("x{number}")),
    )
}

fn map_entry(row: &Value) -> Option<Value> {
    let id = row["id"].as_str()?.trim();
    if id.is_empty() {
        return None;
    }
    let raw_name = row["name"]
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(id);
    let (name, rate) = split_rate(raw_name);
    let caps = row["capabilities"].as_object();
    let flag = |key: &str| {
        caps.and_then(|caps| caps.get(key))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    let modal = |kind: &str| {
        row["capabilities"]["input_modalities"]
            .as_array()
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.as_str()
                        .is_some_and(|text| text.eq_ignore_ascii_case(kind))
                })
            })
    };
    let image = row["type"] == "image"
        || row
            .get("output_modalities")
            .or_else(|| row.pointer("/capabilities/output_modalities"))
            .and_then(Value::as_array)
            .is_some_and(|items| items.iter().any(|item| item == "image"));
    let mut item = json!({ "id": id, "name": name,
        "protocol": normalize_protocol(row["protocol"].as_str()),
        "supportsToolCall": !image && (flag("function_calling") || caps.is_none()),
        "supportsReasoning": !image && (flag("reasoning") || id.to_ascii_lowercase().contains("spark-x")) });
    if image {
        item["kind"] = json!("image");
        item["output_modalities"] = json!(["image"]);
        item["supports_image_editing"] = json!(true);
        item["isDefault"] = json!(false);
    }
    if flag("reasoning")
        || row["reasoning_efforts"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    {
        let levels: Vec<Value> = row["reasoning_efforts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|value| {
                value
                    .as_str()
                    .is_some_and(|value| matches!(value, "low" | "medium" | "high"))
            })
            .cloned()
            .collect();
        item["supportsReasoning"] = json!(!image);
        if !levels.is_empty() {
            item["reasoningEffortLevels"] = json!(levels);
        }
    }
    if flag("vision") || modal("image") {
        item["supportsImages"] = json!(true);
    }
    if modal("video") {
        item["supportsVideo"] = json!(true);
    }
    if let Some(rate) = rate {
        item["credits"] = json!(rate);
    }
    for (target, keys) in [
        (
            "maxInputTokens",
            &[
                "context_length",
                "context_window",
                "input_token_limit",
                "max_input_tokens",
                "maxInputTokens",
            ][..],
        ),
        (
            "maxOutputTokens",
            &[
                "max_output_tokens",
                "max_completion_tokens",
                "max_tokens",
                "maxOutputTokens",
            ][..],
        ),
    ] {
        if let Some(value) = keys
            .iter()
            .find_map(|key| row[*key].as_i64().filter(|value| *value > 0))
        {
            item[target] = json!(value);
        }
    }
    Some(item)
}
