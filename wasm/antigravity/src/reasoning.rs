//! 三态思考处理来自原生 outbound：显式关闭不注入，数字预算保留输出余量；Argon用等级。

use super::{argon, model_rules};
use agent2api_wasm_sdk::{json, Value};

pub fn config(chat: &Value, model: &str, entry: Option<&Value>, image: bool) -> Value {
    if image {
        return json!({ "temperature": 1.0, "topP": 0.95, "topK": 40, "maxOutputTokens": 8192 });
    }
    let mut config = json!({ "topK": 40, "topP": 1.0 });
    for (source, target) in [
        ("temperature", "temperature"),
        ("top_p", "topP"),
        ("top_k", "topK"),
    ] {
        if let Some(value) = chat.get(source).filter(|v| v.is_number()) {
            config[target] = value.clone();
        }
    }
    let client_max = ["max_tokens", "max_completion_tokens"]
        .iter()
        .find_map(|key| chat[*key].as_i64().filter(|value| *value > 0));
    if argon::is_model(model) {
        if let Some(thinking) = argon::thinking_config(chat, model) {
            config["thinkingConfig"] = thinking;
        }
        if let Some(max) = client_max {
            config["maxOutputTokens"] = json!(max.min(65_536));
        }
        return config;
    }
    if let Some(mut budget) = budget(chat, entry) {
        if model.to_ascii_lowercase().starts_with("claude-") {
            budget = budget.max(1024);
        }
        let advertised = entry
            .and_then(|entry| entry["maxOutputTokens"].as_i64())
            .filter(|limit| *limit > 0);
        let limit = advertised.unwrap_or(64_000).min(64_000);
        let max = client_max
            .unwrap_or_else(|| budget.saturating_add(8192))
            .min(limit);
        let minimum = if model.to_ascii_lowercase().starts_with("claude-") {
            1024
        } else {
            1
        };
        let budget = budget.min(max.saturating_sub(1));
        if budget >= minimum {
            config["thinkingConfig"] = json!({ "includeThoughts": true, "thinkingBudget": budget });
        }
        config["maxOutputTokens"] = json!(max);
    } else if let Some(max) = client_max {
        config["maxOutputTokens"] = json!(max.min(64_000));
    }
    config
}

fn budget(chat: &Value, entry: Option<&Value>) -> Option<i64> {
    if let Some(level) = model_rules::read_client_level(chat) {
        if model_rules::reasoning_is_off(&level) {
            return None;
        }
        if let Some(rank) = model_rules::reasoning_rank(&level) {
            return Some(match rank {
                0 | 1 => 1000,
                2 => 4000,
                _ => 10_000,
            });
        }
    }
    let entry = entry?;
    if let Some(budget) = entry["thinkingBudget"].as_i64().filter(|n| *n > 0) {
        return Some(budget);
    }
    (entry["supportsThinking"] == true).then_some(32_768)
}

pub fn patch(params: &Value) -> Value {
    if model_rules::read_client_level(&params["body"]).is_some() {
        return json!({ "action": "skip", "reason": "客户端请求体里已指定思考档位" });
    }
    let level = params["level"].as_str().unwrap_or("").trim();
    if model_rules::reasoning_rank(level).is_none() {
        json!({ "action": "skip", "reason": "该等级不在本家接受的档位内" })
    } else {
        json!({ "action": "set", "field": "reasoning_effort", "value": level })
    }
}
