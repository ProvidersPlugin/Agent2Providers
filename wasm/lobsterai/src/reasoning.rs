//! 目录里的思考档位映射到官方 lobsterai_options；无档位表时保留标准参数。
//! Kimi 采样与工具历史遵循官方兼容层，只有 kimi-k3 本身固定最高思考。

use agent2api_wasm_sdk::{json, Error, Result, Value};

const LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];

pub fn profile(value: &Value) -> Option<Value> {
    let options = value["options"].as_array()?;
    if options.is_empty() || options.len() > LEVELS.len() {
        return None;
    }
    let mut result: Vec<Value> = Vec::new();
    for option in options {
        let level = option["level"].as_str()?;
        let wire = option["openclawLevel"].as_str()?;
        if !LEVELS.contains(&level)
            || !LEVELS.contains(&wire)
            || (level == "off") != (wire == "off")
            || result
                .iter()
                .any(|old| old["level"] == level || old["openclawLevel"] == wire)
        {
            return None;
        }
        result.push(json!({ "level": level, "openclawLevel": wire }));
    }
    let default = value["defaultLevel"].as_str()?;
    if !result.iter().any(|option| option["level"] == default) {
        return None;
    }
    if value
        .get("requestOptionsVersion")
        .is_some_and(|version| !version.is_null() && version != 1)
    {
        return None;
    }
    Some(json!({ "options": result, "defaultLevel": default, "requestOptionsVersion": 1 }))
}

// 来自公开同类适配的逐模型记录；没有目录档位表才使用，不合并不同方言。
fn measured(model: &str) -> &'static [&'static str] {
    match model {
        "kimi-k3" => &["max"],
        "kimi-k3.1-pro"
        | "kimi-k2.8-preview"
        | "qwen3.8-max"
        | "qwen3.8-flash"
        | "qwen3.8-omni-flash"
        | "doubao-seed-2-1-pro-260915" => &["none", "low", "medium", "high", "xhigh", "max"],
        "MiniMax-M3.1-Flash" => &["medium", "high", "xhigh", "max"],
        "qwen3.7-max" | "qwen3.7-plus" => &["none", "low", "medium", "high", "xhigh"],
        "MiniMax-M3" => &["none", "max"],
        _ => &[],
    }
}

pub fn levels(model: &str, profile: &Value) -> Vec<String> {
    if let Some(options) = profile["options"].as_array() {
        return options
            .iter()
            .filter_map(|option| option["openclawLevel"].as_str().map(str::to_string))
            .collect();
    }
    measured(model)
        .iter()
        .map(|level| (*level).to_string())
        .collect()
}

pub fn default_level(model: &str, profile: &Value) -> String {
    let default = profile["defaultLevel"].as_str().unwrap_or("");
    profile["options"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|option| option["level"] == default)
        .and_then(|option| option["openclawLevel"].as_str())
        .unwrap_or_else(|| {
            if model.eq_ignore_ascii_case("kimi-k3") {
                "max"
            } else {
                ""
            }
        })
        .to_string()
}

fn requested(body: &Value, params: &Value) -> Option<String> {
    for value in [
        &body["lobsterai_options"]["thinking"]["level"],
        &body["reasoning_effort"],
        &body["reasoningEffort"],
        &body["thinking"]["level"],
        &params["reasoning"],
    ] {
        if let Some(level) = value
            .as_str()
            .map(str::trim)
            .filter(|level| !level.is_empty())
        {
            return Some(level.to_ascii_lowercase());
        }
    }
    match body["thinking"]["type"].as_str() {
        Some("disabled") => Some("off".into()),
        Some("enabled" | "adaptive") => Some("high".into()),
        _ => None,
    }
}

pub fn apply(body: &mut Value, params: &Value, model: &Value) -> Result<()> {
    let id = body["model"].as_str().unwrap_or("").to_string();
    let lower = id.to_ascii_lowercase();
    let request = requested(body, params);
    if lower.starts_with("kimi-") {
        for field in [
            "temperature",
            "top_p",
            "n",
            "presence_penalty",
            "frequency_penalty",
        ] {
            body.as_object_mut()
                .ok_or_else(|| Error::http(400, "LobsterAI 请求体格式无效"))?
                .remove(field);
        }
    }
    if lower == "kimi-k3" || lower.starts_with("kimi-k3.") || lower.starts_with("kimi-k3-") {
        if let Some(messages) = body["messages"].as_array_mut() {
            for message in messages {
                if message["role"] == "assistant"
                    && message["tool_calls"]
                        .as_array()
                        .is_some_and(|calls| !calls.is_empty())
                    && message.get("reasoning_content").is_none_or(Value::is_null)
                {
                    message["reasoning_content"] = json!("");
                }
            }
        }
    }
    if lower == "kimi-k3" {
        let object = body
            .as_object_mut()
            .ok_or_else(|| Error::http(400, "LobsterAI 请求体格式无效"))?;
        for field in ["thinking", "reasoningEffort", "lobsterai_options"] {
            object.remove(field);
        }
        object.insert("reasoning_effort".into(), json!("max"));
        return Ok(());
    }
    if model["supportsReasoning"] == false && request.is_none() {
        return Ok(());
    }
    let profile = &model["thinkingProfile"];
    if let Some(options) = profile["options"]
        .as_array()
        .filter(|options| !options.is_empty())
    {
        let wanted = request.as_deref().map(|level| {
            if matches!(level, "none" | "disabled") {
                "off"
            } else {
                level
            }
        });
        let selected = match wanted {
            Some(level) => options
                .iter()
                .find(|option| option["level"] == level || option["openclawLevel"] == level)
                .ok_or_else(|| Error::http(400, "LobsterAI 当前模型不支持所选思考档位"))?,
            None => options
                .iter()
                .find(|option| option["level"] == profile["defaultLevel"])
                .ok_or_else(|| Error::http(502, "LobsterAI 思考目录缺少默认档位"))?,
        };
        let level = selected["level"]
            .as_str()
            .ok_or_else(|| Error::http(502, "LobsterAI 思考档位无效"))?;
        let object = body
            .as_object_mut()
            .ok_or_else(|| Error::http(400, "LobsterAI 请求体格式无效"))?;
        for field in ["thinking", "reasoning_effort", "reasoningEffort"] {
            object.remove(field);
        }
        object.insert(
            "lobsterai_options".into(),
            json!({ "version": 1, "thinking": { "level": level } }),
        );
    } else if let Some(level) = request {
        if body.get("lobsterai_options").is_some() {
            return Err(Error::http(
                400,
                "LobsterAI 当前模型没有私有思考档位表，请使用标准思考参数",
            ));
        }
        let level = if matches!(level.as_str(), "off" | "disabled") {
            "none"
        } else {
            &level
        };
        let allowed = measured(&id);
        if !allowed.is_empty() && !allowed.contains(&level) {
            return Err(Error::http(400, "LobsterAI 当前模型不支持所选思考档位"));
        }
        let object = body
            .as_object_mut()
            .ok_or_else(|| Error::http(400, "LobsterAI 请求体格式无效"))?;
        object.remove("thinking");
        object.remove("reasoningEffort");
        object.insert("reasoning_effort".into(), json!(level));
    }
    Ok(())
}
