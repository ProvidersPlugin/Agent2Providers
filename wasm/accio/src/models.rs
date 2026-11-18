//! 目录纯数据规则来自原生 accio/models.rs；仅国际目录，不合并国内缓存。
//! 上游展示名映射旧 id，真实发送使用 upstreamKey；不按新型号偷偷替换旧型号。

use crate::util;
use agent2api_wasm_sdk::{json, Error, Result, Value};

const FALLBACK: &[(&str, &str, u64, bool, &[&str], bool, &str)] = &[
    (
        "gemini-3-flash-preview",
        "Gemini 3 Flash",
        1_000_000,
        true,
        &["low", "high"],
        true,
        "top",
    ),
    (
        "gemini-3.1-pro-preview",
        "Gemini 3.1 Pro",
        1_000_000,
        true,
        &["low", "high"],
        false,
        "top",
    ),
    (
        "qwen3.6-plus",
        "Qwen 3.6 Plus",
        991_808,
        false,
        &[],
        false,
        "top",
    ),
    (
        "qwen3-max-2026-01-23",
        "Qwen 3 Max",
        262_144,
        false,
        &[],
        false,
        "top",
    ),
    (
        "gpt-5.4",
        "GPT 5.4",
        1_050_000,
        true,
        &["low", "high"],
        false,
        "properties",
    ),
    (
        "gpt-5.2-1211",
        "GPT 5.2",
        400_000,
        true,
        &["low", "high"],
        false,
        "properties",
    ),
    (
        "claude-sonnet-4-6",
        "Claude Sonnet 4.6",
        1_000_000,
        true,
        &["low", "medium", "high", "max"],
        false,
        "top",
    ),
    (
        "claude-opus-4-6",
        "Claude Opus 4.6",
        1_000_000,
        true,
        &["low", "medium", "high", "max"],
        false,
        "top",
    ),
    ("glm-5", "GLM-5", 200_000, false, &[], false, "top"),
    (
        "MiniMax-M2.5",
        "MiniMax M2.5",
        204_800,
        true,
        &[],
        false,
        "properties",
    ),
];

const STABLE_IDS: &[(&str, &str)] = &[
    ("claude-sonnet-4-6", "Claude Sonnet 4.6"),
    ("claude-opus-4-6", "Claude Opus 4.6"),
    ("gemini-3-flash-preview", "Gemini 3 Flash"),
    ("gemini-3.1-pro-preview", "Gemini 3.1 Pro"),
    ("glm-5", "GLM-5"),
];

fn stable_id(code: &str, display: &str) -> String {
    STABLE_IDS
        .iter()
        .find(|(_, name)| name.eq_ignore_ascii_case(display))
        .map(|(id, _)| id.to_string())
        .unwrap_or_else(|| code.to_string())
}

fn entry(raw: &Value) -> Option<Value> {
    let code = util::text(raw, &["modelCode", "modelName"]);
    if code.is_empty() || code.len() > 512 || code.chars().any(char::is_control) {
        return None;
    }
    let display = util::text(raw, &["modelDisplayName"]);
    let display = if display.is_empty() {
        code.clone()
    } else {
        display.chars().take(256).collect()
    };
    let multimodal = raw["multimodal"].as_bool().unwrap_or(false);
    let mut efforts: Vec<String> = raw["reasoningEfforts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|value| {
            !value.trim().is_empty() && value.len() <= 32 && !value.chars().any(char::is_control)
        })
        .take(32)
        .map(|value| value.trim().to_string())
        .collect();
    let default_effort = raw["defaultReasoningEffort"]
        .as_str()
        .map(str::trim)
        .filter(|value| {
            !value.is_empty() && value.len() <= 32 && !value.chars().any(char::is_control)
        });
    if efforts.is_empty() {
        if let Some(default) = default_effort {
            efforts.push(default.into());
        }
    }
    let mut entry = json!({ "id": stable_id(&code, &display), "name": display, "upstreamKey": code,
        "supportsImages": multimodal, "supportsToolCall": true, "isDefault": raw["isDefault"] == true, "kind": "chat",
        "inputModalities": if multimodal { vec!["text", "image"] } else { vec!["text"] }, "reasoningEfforts": efforts,
        "protocolKind": if matches!(util::text(raw, &["protocol"]).to_ascii_lowercase().as_str(), "openai" | "responses") { "properties" } else { "top" } });
    if !efforts.is_empty() {
        entry["supportsReasoning"] = json!(true);
        entry["reasoningEffortLevels"] = json!(efforts);
    }
    if let Some(default) = default_effort {
        entry["defaultReasoningEffort"] = json!(default);
    }
    if let Some(window) = raw["contextWindow"].as_u64().filter(|window| *window > 0) {
        entry["maxInputTokens"] = json!(window);
    }
    if let Some(description) = raw["modelDesc"].as_str() {
        entry["modelDesc"] = json!(description.chars().take(1000).collect::<String>());
    }
    if let Some(output) = raw["maxTokens"].as_u64().filter(|value| *value > 0) {
        entry["maxOutputTokens"] = json!(output);
    }
    if let Some(multiple) = raw
        .get("effectiveMultiple")
        .or_else(|| raw.get("usageMultiple"))
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0)
    {
        entry["usageMultiple"] = json!(multiple);
    }
    Some(entry)
}

pub fn fallback() -> Vec<Value> {
    FALLBACK
        .iter()
        .filter_map(
            |(code, name, window, images, efforts, default, placement)| {
                let mut row = entry(
                    &json!({ "modelCode": code, "modelDisplayName": name, "contextWindow": window,
            "multimodal": images, "reasoningEfforts": efforts, "isDefault": default }),
                )?;
                row["protocolKind"] = json!(placement);
                Some(row)
            },
        )
        .collect()
}

pub fn parse_checked(payload: &Value) -> Result<Vec<Value>> {
    let root = payload.get("data").unwrap_or(payload);
    let providers = root
        .as_array()
        .or_else(|| root["providers"].as_array())
        .ok_or_else(|| Error::http(502, "Accio 模型目录缺少 provider 数组，未当作空清单"))?;
    if providers.len() > 512
        || providers.iter().any(|provider| {
            provider["modelList"].as_array().is_none_or(|rows| {
                rows.len() > 4096
                    || rows.iter().any(|row| {
                        if !row.is_object() {
                            return true;
                        }
                        let non_chat = row["visible"] == false
                            || row["supportedOperationList"]
                                .as_array()
                                .is_some_and(|values| {
                                    values.iter().filter_map(Value::as_str).any(|value| {
                                        ["VIDEO_GENERATION", "IMAGE_GENERATION", "AUDIO_GENERATION"]
                                            .iter()
                                            .any(|known| value.eq_ignore_ascii_case(known))
                                    })
                                });
                        !non_chat && entry(row).is_none()
                    })
            })
        })
    {
        return Err(Error::http(502, "Accio 模型目录条目形状无效，保留原清单"));
    }
    Ok(parse(payload))
}

pub fn parse(payload: &Value) -> Vec<Value> {
    let root = payload.get("data").unwrap_or(payload);
    let providers = root.as_array().or_else(|| root["providers"].as_array());
    let mut models = Vec::<Value>::new();
    for provider in providers.into_iter().flatten() {
        let Some(rows) = provider["modelList"].as_array() else {
            continue;
        };
        for row in rows {
            if row["visible"] == false
                || row["supportedOperationList"]
                    .as_array()
                    .is_some_and(|operations| {
                        operations
                            .iter()
                            .filter_map(Value::as_str)
                            .any(|operation| {
                                ["VIDEO_GENERATION", "IMAGE_GENERATION", "AUDIO_GENERATION"]
                                    .iter()
                                    .any(|blocked| operation.eq_ignore_ascii_case(blocked))
                            })
                    })
            {
                continue;
            }
            let Some(row) = entry(row) else {
                continue;
            };
            if models.iter().any(|existing| {
                existing["id"]
                    .as_str()
                    .unwrap_or("")
                    .eq_ignore_ascii_case(row["id"].as_str().unwrap_or(""))
            }) {
                continue;
            }
            models.push(row);
            if models.len() >= 512 {
                return models;
            }
        }
    }
    models
}

pub fn saved_rows(value: &Value) -> Option<Vec<Value>> {
    let rows = value.as_array().filter(|rows| rows.len() <= 512)?;
    let mut result = Vec::with_capacity(rows.len());
    for row in rows {
        let id = row["id"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 512 && !id.chars().any(char::is_control))?;
        let key = row["upstreamKey"].as_str().filter(|key| {
            !key.is_empty() && key.len() <= 512 && !key.chars().any(char::is_control)
        })?;
        let mut normalized = json!({ "id": id, "upstreamKey": key, "name": util::text(row, &["name"]),
            "supportsImages": row["supportsImages"].as_bool().unwrap_or(false), "supportsToolCall": true,
            "kind": "chat", "isDefault": row["isDefault"] == true,
            "protocolKind": if row["protocolKind"] == "properties" { "properties" } else { "top" },
            "reasoningEfforts": row["reasoningEfforts"].as_array().cloned().unwrap_or_default() });
        for key in [
            "supportsReasoning",
            "maxInputTokens",
            "maxOutputTokens",
            "modelDesc",
            "usageMultiple",
            "reasoningEffortLevels",
            "defaultReasoningEffort",
            "inputModalities",
        ] {
            if let Some(value) = row.get(key) {
                normalized[key] = value.clone();
            }
        }
        result.push(normalized);
    }
    Some(result)
}

pub fn resolve(rows: &[Value], wanted: &str) -> Option<Value> {
    let wanted = wanted.trim();
    if wanted.is_empty() {
        return None;
    }
    rows.iter()
        .find(|row| {
            ["id", "name", "upstreamKey"].iter().any(|key| {
                row[*key]
                    .as_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case(wanted))
            })
        })
        .cloned()
}
