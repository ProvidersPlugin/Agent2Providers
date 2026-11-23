//! Qoder 中国版静态兜底与远程 chat + app 目录解析；不带国际/千问模型表。
//! enable 是账号权益而非可见性，保留未启用条目，但公开目录不声称账号可用。
use super::{context, endpoints, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub const MAX_OUTPUT: i64 = 32_768;
const DEFAULT_CONTEXT: i64 = 200_000;

pub fn fallback() -> Vec<Value> {
    let rows: &[(&str, &str, bool, &[&str], bool, bool, &str)] = &[
        (
            "Qwen3.8-Flash",
            "qfmodel",
            true,
            &["low", "medium", "xhigh"],
            true,
            true,
            "0.1",
        ),
        (
            "Qwen3.8-Max",
            "qmodel_38max",
            true,
            &["low", "medium", "xhigh"],
            true,
            true,
            "0.5",
        ),
        ("Auto", "auto", true, &[], true, false, "1"),
        (
            "Qwen3.7-Max",
            "qmodel_latest",
            true,
            &[],
            true,
            false,
            "0.5",
        ),
        ("Qwen3.7-Plus", "qmodel", true, &[], false, false, "0.1"),
        ("DeepSeek-V4-Pro", "dmodel", true, &[], false, false, "0.8"),
        ("DeepSeek-Flash", "dfmodel", false, &[], false, false, "0.2"),
        ("GLM-5.3", "gmodel", true, &[], true, false, "0.6"),
        ("Kimi-K2.8-Preview", "kmodel", true, &[], true, false, "0.3"),
        ("MiniMax-M3", "mmodel", false, &[], false, false, "0.2"),
    ];
    rows.iter()
        .map(|(id, key, reasoning, efforts, vision, enabled, credits)| {
            let mut model = entry(id, key, *reasoning, *vision, DEFAULT_CONTEXT);
            model["efforts"] = json!(efforts);
            model["reasoningEffortLevels"] = json!(efforts);
            model["accountEnabled"] = json!(enabled);
            model["permissionKnown"] = json!(false);
            model["credits"] = json!(format!("x{credits} credits"));
            model
        })
        .collect()
}
fn visibility(id: &str) -> &'static str {
    if ["Auto", "Ultimate", "Performance", "Efficient"]
        .iter()
        .any(|value| value.eq_ignore_ascii_case(id))
        || id.to_ascii_lowercase().starts_with("qwen3.7")
        || id.to_ascii_lowercase().contains("k2.8-preview")
    {
        "hidden"
    } else {
        "shown"
    }
}
fn entry(id: &str, key: &str, reasoning: bool, vision: bool, window: i64) -> Value {
    json!({ "id": id, "name": id, "upstreamKey": key, "providerId": endpoints::PROVIDER,
        "visibility": visibility(id), "reasoning": reasoning, "supportsReasoning": reasoning,
        "supportsImages": vision, "supportsToolCall": true, "efforts": [], "reasoningEffortLevels": [],
        "maxInputTokens": window, "maxOutputTokens": MAX_OUTPUT, "isDefault": false, "kind": "chat",
        "config": { "key": key, "is_reasoning": reasoning, "is_vl": vision, "source": "system" } })
}
fn valid_name(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
}
pub fn parse(payload: &Value) -> Result<Vec<Value>> {
    for key in ["chat", "app"] {
        if payload.get(key).is_some_and(|value| !value.is_array()) {
            return Err(Error::http(
                502,
                "Qoder 模型目录字段格式无效，已保留现有清单",
            ));
        }
    }
    if !payload.get("chat").is_some_and(Value::is_array)
        && !payload.get("app").is_some_and(Value::is_array)
    {
        return Err(Error::http(
            502,
            "Qoder 模型目录缺少 chat/app 数组，已保留现有清单",
        ));
    }
    let mut rows = payload["chat"].as_array().cloned().unwrap_or_default();
    let mut keys = rows
        .iter()
        .map(|row| util::text(row, &["key"]))
        .filter(|key| !key.is_empty())
        .collect::<Vec<_>>();
    for row in payload["app"].as_array().into_iter().flatten() {
        let key = util::text(row, &["key"]);
        if key.is_empty() || keys.contains(&key) || row["format"] != "openai" {
            continue;
        }
        keys.push(key);
        rows.push(row.clone());
    }
    let empty = rows.is_empty();
    if rows.len() > 2048 {
        return Err(Error::http(502, "Qoder 模型目录超过受控大小"));
    }
    let mut models = Vec::new();
    let mut seen = Vec::new();
    for row in rows {
        let key = util::text(&row, &["key"]);
        let display = util::text(&row, &["display_name"]);
        let id: String = display.chars().filter(|ch| !ch.is_whitespace()).collect();
        if !valid_name(&display)
            || !valid_name(&id)
            || !util::printable(&key, 256)
            || key.is_empty()
        {
            continue;
        }
        let lower = id.to_lowercase();
        if seen.contains(&lower) {
            continue;
        }
        seen.push(lower);
        let reasoning = util::truthy(&row["is_reasoning"]) || util::truthy(&row["thinking_config"]);
        let tiers = context::tiers(&row);
        let window = tiers
            .iter()
            .filter_map(|tier| tier["tokens"].as_i64())
            .max()
            .or_else(|| {
                row["max_input_tokens"]
                    .as_i64()
                    .filter(|tokens| *tokens > 0)
            })
            .unwrap_or(DEFAULT_CONTEXT);
        let mut model = entry(&id, &key, reasoning, util::truthy(&row["is_vl"]), window);
        model["visibility"] = json!("shown");
        model["name"] = json!(display);
        model["permissionKnown"] = json!(row.get("enable").is_some_and(|value| !value.is_null()));
        model["accountEnabled"] = json!(util::truthy(&row["enable"]));
        let efforts = row
            .pointer("/thinking_config/enabled/efforts")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|values| values.keys())
            .filter(|value| util::printable(value, 64))
            .cloned()
            .collect::<Vec<_>>();
        model["efforts"] = json!(efforts);
        model["reasoningEffortLevels"] = json!(efforts);
        if let Some(price) = util::number(&row["price_factor"]).filter(|price| *price >= 0.0) {
            let text = format!("{price:.2}");
            let text = text.trim_end_matches('0').trim_end_matches('.');
            model["credits"] = json!(format!("x{text} credits"));
        }
        if let Some(source) = row["source"]
            .as_str()
            .filter(|value| !value.is_empty() && util::printable(value, 128))
        {
            model["config"]["source"] = json!(source);
        }
        if let Some(format) = row["format"]
            .as_str()
            .filter(|value| !value.is_empty() && util::printable(value, 64))
        {
            model["config"]["format"] = json!(format);
        }
        if !tiers.is_empty() {
            model["config"]["tiers"] = json!(tiers);
        }
        if let Some(current) = row["max_input_tokens"].as_i64().filter(|value| *value > 0) {
            model["config"]["max_input_tokens"] = json!(current);
        }
        models.push(model);
    }
    if models.is_empty() && !empty {
        return Err(Error::http(
            502,
            "Qoder 模型目录没有可解析的条目，已保留现有清单",
        ));
    }
    Ok(models)
}
pub fn public(models: &[Value]) -> Vec<Value> {
    models
        .iter()
        .map(|model| {
            let mut model = model.clone();
            if let Some(fields) = model.as_object_mut() {
                for key in ["accountEnabled", "permissionKnown", "config"] {
                    fields.remove(key);
                }
            }
            model
        })
        .collect()
}
pub fn find(models: &[Value], name: &str) -> Option<Value> {
    for key in ["id", "name", "upstreamKey"] {
        if let Some(model) = models.iter().find(|model| {
            model[key]
                .as_str()
                .is_some_and(|value| value.trim().eq_ignore_ascii_case(name))
        }) {
            return Some(model.clone());
        }
    }
    None
}
