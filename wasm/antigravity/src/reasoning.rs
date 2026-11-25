//! 三态思考处理：显式关闭不注入；Gemini 4 / Claude 5+ 走 thinkingLevel 枚举，
//! 传统模型沿用数字 thinkingBudget 并保留输出余量。

use super::{catalog_shape, model_rules, tiers};
use agent2api_wasm_sdk::{self as sdk, json, Value};

pub fn config(
    chat: &Value,
    model: &str,
    entry: Option<&Value>,
    variant: Option<&Value>,
    resolved_level: Option<&str>,
    image: bool,
) -> Value {
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
    let advertised_max = variant
        .and_then(|v| v["maxOutputTokens"].as_i64())
        .filter(|limit| *limit > 0)
        .or_else(|| {
            entry
                .and_then(|e| e["maxOutputTokens"].as_i64())
                .filter(|limit| *limit > 0)
        });

    let client_effort = model_rules::read_client_level(chat);
    if client_effort
        .as_deref()
        .is_some_and(model_rules::reasoning_is_off)
    {
        if let Some(max) = client_max {
            config["maxOutputTokens"] = json!(max.min(advertised_max.unwrap_or(65_536)));
        }
        return config;
    }

    if entry.is_some_and(|e| e["supportsThinking"] == false && e["supportsReasoning"] == false) {
        if let Some(max) = client_max {
            config["maxOutputTokens"] = json!(max.min(advertised_max.unwrap_or(65_536)));
        }
        return config;
    }

    let use_level = if let Some(variant) = variant {
        tiers::uses_thinking_level(model, Some(variant))
    } else {
        tiers::uses_thinking_level(model, entry)
    };
    if use_level {
        let upstream_level = variant
            .and_then(|v| v["thinkingLevel"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                resolved_level
                    .or(client_effort.as_deref())
                    .or_else(|| tiers::strip_effort_id(model, false).1)
                    .map(|lvl| tiers::to_upstream_thinking_level(lvl).to_string())
            })
            .unwrap_or_else(|| "MEDIUM".to_string());
        config["thinkingConfig"] = json!({
            "includeThoughts": true,
            "thinkingLevel": upstream_level,
        });
        if let Some(max) = client_max {
            config["maxOutputTokens"] = json!(max.min(advertised_max.unwrap_or(65_536)));
        }
        return config;
    }

    if let Some(mut budget) = budget(client_effort.as_deref(), resolved_level, entry, variant) {
        if model.to_ascii_lowercase().starts_with("claude-") {
            budget = budget.max(1024);
        }
        let limit = advertised_max.unwrap_or(64_000);
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
        config["maxOutputTokens"] = json!(max.min(advertised_max.unwrap_or(64_000)));
    }
    config
}

fn budget(
    client_effort: Option<&str>,
    resolved_level: Option<&str>,
    entry: Option<&Value>,
    variant: Option<&Value>,
) -> Option<i64> {
    if let Some(variant_budget) = variant
        .and_then(|v| v["thinkingBudget"].as_i64())
        .filter(|n| *n > 0)
    {
        return Some(variant_budget);
    }
    if let Some(level) = resolved_level.or(client_effort) {
        if model_rules::reasoning_is_off(level) {
            return None;
        }
        if let Some(rank) = model_rules::reasoning_rank(level) {
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

fn cached_model_entry(model: &str) -> Option<Value> {
    if model.is_empty() {
        return None;
    }
    if let Ok(shared) = sdk::catalog::cached() {
        if let Some(models) = shared["models"].as_array() {
            if let Some(found) = catalog_shape::entry(models, model) {
                return Some(found);
            }
        }
    }
    catalog_shape::entry(&catalog_shape::fallback(), model)
}

pub fn patch(params: &Value) -> Value {
    if model_rules::read_client_level(&params["body"]).is_some() {
        return json!({ "action": "skip", "reason": "客户端请求体里已指定思考档位" });
    }
    let raw_level = params["level"].as_str().unwrap_or("").trim();
    if model_rules::reasoning_is_off(raw_level) {
        return json!({ "action": "skip", "reason": "映射绑定不注入关闭思考字段" });
    }
    let Some(normalized) = tiers::normalize_level(raw_level) else {
        return json!({ "action": "skip", "reason": "该等级不在本家接受的档位内" });
    };
    let model = params["model"].as_str().unwrap_or("").trim();
    if let Some(entry) = cached_model_entry(model) {
        if entry["supportsThinking"] == false && entry["supportsReasoning"] == false {
            return json!({ "action": "skip", "reason": "该模型不支持思考档位" });
        }
        if let Some(levels) = entry["reasoningLevels"].as_array() {
            let declared: Vec<String> = levels
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if !declared.is_empty() {
                if let Some(effective) = tiers::pick_closest_level(&declared, normalized) {
                    return json!({ "action": "set", "field": "reasoning_effort", "value": effective });
                }
            }
        }
    }
    json!({ "action": "set", "field": "reasoning_effort", "value": normalized })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_uses_thinking_level_for_claude_5_and_gemini_4_models() {
        let variant = json!({
            "id": "claude-opus-5-5-high",
            "thinkingMode": "level",
            "thinkingLevel": "HIGH",
            "maxOutputTokens": 128000,
        });
        let entry = json!({
            "id": "claude-opus-5-5",
            "supportsThinking": true,
            "maxOutputTokens": 128000,
            "antigravityThinkingMode": "level",
        });
        let chat = json!({ "model": "claude-opus-5-5", "max_tokens": 100000 });
        let out = config(
            &chat,
            "claude-opus-5-5-high",
            Some(&entry),
            Some(&variant),
            Some("high"),
            false,
        );
        assert_eq!(out["thinkingConfig"]["includeThoughts"], json!(true));
        assert_eq!(out["thinkingConfig"]["thinkingLevel"], json!("HIGH"));
        assert!(out["thinkingConfig"].get("thinkingBudget").is_none());
        assert_eq!(out["maxOutputTokens"], json!(100000));
    }

    #[test]
    fn config_uses_xhigh_level_for_hydrogen_tiered_xhigh() {
        let variant = json!({
            "id": "gemini-4-hydrogen-tiered-xhigh",
            "thinkingMode": "level",
            "thinkingLevel": "XHIGH",
            "maxOutputTokens": 262144,
        });
        let chat = json!({ "model": "gemini-4-hydrogen-tiered", "reasoning_effort": "xhigh" });
        let out = config(
            &chat,
            "gemini-4-hydrogen-tiered-xhigh",
            None,
            Some(&variant),
            Some("xhigh"),
            false,
        );
        assert_eq!(out["thinkingConfig"]["thinkingLevel"], json!("XHIGH"));
        assert!(out["thinkingConfig"].get("thinkingBudget").is_none());
    }

    #[test]
    fn config_respects_variant_budget_for_legacy_tiered_models() {
        let variant = json!({
            "id": "gemini-3.8-flash-low",
            "thinkingMode": "budget",
            "thinkingBudget": 1000,
            "maxOutputTokens": 65536,
        });
        let chat = json!({ "model": "gemini-3.8-flash", "reasoning_effort": "low" });
        let out = config(
            &chat,
            "gemini-3.8-flash-low",
            None,
            Some(&variant),
            Some("low"),
            false,
        );
        assert_eq!(out["thinkingConfig"]["thinkingBudget"], json!(1000));
        assert!(out["thinkingConfig"].get("thinkingLevel").is_none());
    }

    #[test]
    fn config_omits_thinking_when_client_turns_it_off() {
        let variant = json!({
            "id": "claude-opus-5-5-medium",
            "thinkingMode": "level",
            "thinkingLevel": "MEDIUM",
        });
        let chat = json!({ "model": "claude-opus-5-5", "reasoning_effort": "off" });
        let out = config(
            &chat,
            "claude-opus-5-5-medium",
            None,
            Some(&variant),
            Some("medium"),
            false,
        );
        assert!(out.get("thinkingConfig").is_none());
    }
}
