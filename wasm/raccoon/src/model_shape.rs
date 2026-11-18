//! 当前目录的name是模型ID，仅chat分类；保留隐藏项、实际/原始倍率及上下文未知值。

use super::{reasoning, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn parse(payload: &Value) -> Result<Vec<Value>> {
    let data = payload
        .get("data")
        .filter(|value| value.is_object())
        .unwrap_or(payload);
    let categories = data["categories"]
        .as_array()
        .ok_or_else(|| Error::http(502, "小浣熊目录缺少有效categories，已保留现有清单"))?;
    let mut ids = std::collections::HashSet::new();
    let mut models = Vec::new();
    let mut has_entries = false;
    for category in categories {
        if !category.is_object() {
            return Err(Error::http(502, "小浣熊模型分类格式无效，已保留现有清单"));
        }
        if category["type"].as_str().is_some_and(|kind| kind != "chat") {
            continue;
        }
        let entries = category["models"]
            .as_array()
            .ok_or_else(|| Error::http(502, "小浣熊对话分类缺少models，已保留现有清单"))?;
        has_entries |= !entries.is_empty();
        for model in entries.iter().filter_map(entry) {
            if ids.insert(model["id"].as_str().unwrap_or("").to_string()) {
                models.push(model);
            }
        }
    }
    if has_entries && models.is_empty() {
        return Err(Error::http(502, "小浣熊目录包含无效模型，已保留现有清单"));
    }
    // 官方成功返回空分类/空models与格式损坏不同，不能把已确认的空目录还原成旧模型。
    Ok(models)
}
fn entry(value: &Value) -> Option<Value> {
    value.as_object()?;
    let name = ["name", "model_name"]
        .iter()
        .map(|key| util::js(&value[*key]).trim().to_string())
        .find(|name| !name.is_empty())?;
    let codename = util::js(&value["description"]).trim().to_string();
    let display = util::text(value, &["display_name"]);
    let display = if !display.is_empty() {
        display
    } else if !codename.is_empty() && codename.chars().count() <= 64 {
        codename
    } else {
        name.clone()
    };
    let tags: Vec<String> = value["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect();
    let images = value
        .get("supports_images")
        .or_else(|| value.get("supportImage"))
        .and_then(Value::as_bool)
        .unwrap_or_else(|| {
            tags.iter().any(|tag| {
                ["vision", "image", "image-understanding"]
                    .contains(&tag.to_ascii_lowercase().as_str())
            })
        });
    let tools = value
        .get("supports_tool_call")
        .or_else(|| value.get("supportToolCall"))
        .and_then(Value::as_bool)
        .unwrap_or_else(|| {
            ["glm-5.3", "sn-glm-5-3"]
                .iter()
                .any(|model| name.eq_ignore_ascii_case(model))
                || tags.iter().any(|tag| {
                    ["function_call", "function-calling", "tool_call"]
                        .contains(&tag.to_ascii_lowercase().as_str())
                })
        });
    let thinking = value
        .get("supports_reasoning")
        .or_else(|| value.get("supportThinking"))
        .and_then(Value::as_bool)
        .unwrap_or_else(|| reasoning::supported(&name));
    let number = |keys: &[&str]| {
        keys.iter().find_map(|key| {
            util::integer(&value[*key])
                .filter(|value| *value > 0)
                .or_else(|| util::integer(&value["params"][*key]).filter(|value| *value > 0))
        })
    };
    let context = number(&["context_window", "contextWindow"]);
    let output = number(&["max_tokens", "maxTokens"]);
    let base = util::number(&value["billing_multiplier"]).filter(|value| *value >= 0.0);
    let rate = util::number(&value["billing_effective_multiplier"])
        .filter(|value| *value >= 0.0)
        .or(base);
    let visible = value["visible"] != false;
    let mut model = json!({ "id": name, "name": display, "displayName": display,
        "description": util::text(value, &["display_description"]), "tags": tags,
        "contextWindow": context, "maxInputTokens": context, "maxTokens": output, "maxOutputTokens": output,
        "supportsImages": images, "supportImage": images, "supportsToolCall": tools,
        "supportsReasoning": thinking, "supportThinking": thinking,
        "visible": visible, "visibility": if visible { "shown" } else { "hidden" }, "pointsMultiplier": base,
        "billingStatus": value["billing_status"], "credits": rate.map(|rate| {
            let text = format!("{rate:.2}"); format!("x{} credits", text.trim_end_matches('0').trim_end_matches('.'))
        }).unwrap_or_default() });
    for (source, target) in [
        ("ability_level", "abilityLevel"),
        ("billing_category", "billingCategory"),
        ("billing_multiplier", "billingMultiplier"),
        ("billing_effective_multiplier", "billingEffectiveMultiplier"),
        ("billing_status_note", "billingStatusNote"),
        ("billing_discounts", "billingDiscounts"),
    ] {
        if let Some(value) = value.get(source) {
            model[target] = value.clone();
        }
    }
    Some(model)
}
