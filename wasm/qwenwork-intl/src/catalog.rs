//! 国际 qwork 目录沿用原生基线的三静态项（非国际实测）：各 131072，无图片或思考声明。
//! 远程有值读真实值，缺省才保留 1024000 原生推断值；不将这份默认表宣传为国际已验证能力。
//! 所有 qwork 项均可展示，enable 是当前账号权益，不是展示开关；不把它当跨账号许可表。
use super::{context, endpoints, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn fallback() -> Vec<Value> {
    [
        ("pro", "高级"),
        ("flash", "标准"),
        ("qwen3.8-max-preview", "Qwen3.8-Max"),
    ]
    .into_iter()
    .map(|(key, id)| {
        let mut model = entry(id, id, key, false, false, endpoints::STATIC_CONTEXT);
        model["supportsReasoningEffort"] = json!(false);
        model["permissionKnown"] = json!(false);
        model["contextSource"] = json!("native_static_inferred");
        model["capabilityEvidence"] = json!("native_baseline_not_internationally_verified");
        model
    })
    .collect()
}
fn entry(id: &str, name: &str, key: &str, reasoning: bool, vision: bool, window: i64) -> Value {
    json!({ "id": id, "name": name, "upstreamKey": key, "providerId": endpoints::PROVIDER,
        "visibility": "shown", "reasoning": reasoning, "supportsReasoning": reasoning,
        "supportsImages": vision, "supportsToolCall": true, "efforts": [], "reasoningEffortLevels": [],
        "maxInputTokens": window, "maxOutputTokens": endpoints::MAX_OUTPUT, "isDefault": false, "kind": "chat",
        "config": { "key": key, "is_reasoning": reasoning, "is_vl": vision, "source": "system", "max_input_tokens": window } })
}
fn valid_name(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
}
pub fn parse(payload: &Value) -> Result<Vec<Value>> {
    let payload = super::client::unwrap_data(payload);
    let rows = payload["qwork"]
        .as_array()
        .filter(|rows| rows.len() <= 2048)
        .ok_or_else(|| Error::http(502, "千问办公未返回有效 qwork 目录，已保留现有清单"))?;
    let mut models = Vec::new();
    let mut seen = Vec::new();
    let mut seen_keys = Vec::new();
    for row in rows {
        let key = util::text(row, &["key"]);
        let display = util::text(row, &["display_name"]);
        let id: String = display.chars().filter(|ch| !ch.is_whitespace()).collect();
        if !valid_name(&display)
            || !valid_name(&id)
            || key.is_empty()
            || !util::printable(&key, 256)
        {
            continue;
        }
        let lower = id.to_lowercase();
        if seen.contains(&lower) || seen_keys.contains(&key) {
            continue;
        }
        seen.push(lower);
        seen_keys.push(key.clone());
        let reasoning = util::truthy(&row["is_reasoning"]) || util::truthy(&row["thinking_config"]);
        let declared = util::integer(&row["max_input_tokens"]).filter(|value| *value > 0);
        // 基线当前无多档上下文；仍保留未来目录真实下发档位的既有解析，而非自己抬大窗口。
        let tiers = context::tiers(row);
        let tier_max = tiers
            .iter()
            .filter_map(|tier| tier["tokens"].as_i64())
            .max();
        let window = tier_max
            .or(declared)
            .unwrap_or(endpoints::REMOTE_CONTEXT_FALLBACK);
        let current = declared.unwrap_or_else(|| {
            tiers
                .iter()
                .find(|tier| tier["isDefault"] == true)
                .or_else(|| tiers.first())
                .and_then(|tier| tier["tokens"].as_i64())
                .unwrap_or(window)
        });
        let mut model = entry(
            &id,
            &display,
            &key,
            reasoning,
            util::truthy(&row["is_vl"]),
            window,
        );
        model["capabilityEvidence"] = json!("remote_catalog");
        model["permissionKnown"] = json!(row.get("enable").is_some_and(|value| !value.is_null()));
        model["accountEnabled"] = json!(util::truthy(&row["enable"]));
        model["contextSource"] = json!(if tier_max.is_some() {
            "remote_tiers"
        } else if declared.is_some() {
            "remote"
        } else {
            "native_default"
        });
        model["config"]["max_input_tokens"] = json!(current);
        let efforts = row
            .pointer("/thinking_config/enabled/efforts")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|values| values.keys())
            .filter(|value| !value.is_empty() && util::printable(value, 64))
            .cloned()
            .collect::<Vec<_>>();
        model["supportsReasoningEffort"] = json!(!efforts.is_empty());
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
        if let Some(maximum) = util::integer(&row["max_output_tokens"]).filter(|value| *value > 0) {
            model["maxOutputTokens"] = json!(maximum);
        }
        if let Some(thinking) = row.get("thinking_config").filter(|value| value.is_object()) {
            model["config"]["thinking_config"] = thinking.clone();
        }
        if !tiers.is_empty() {
            model["config"]["tiers"] = json!(tiers);
        }
        models.push(model);
    }
    if models.is_empty() && !rows.is_empty() {
        return Err(Error::http(502, "千问办公目录条目全部无效，已保留现有清单"));
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
