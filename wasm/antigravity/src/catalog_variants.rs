//! 思考等级变体的目录合并与请求路由解析（从 `catalog_shape.rs` 拆出以保持单文件精简）。
//! 同一基础模型下的多个思考等级条目合并为一条，并保留上游真实变体映射供转发按档位选路。

use super::{aliases, argon, catalog_shape, model_rules, tiers, util};
use agent2api_wasm_sdk::{json, Value};
use serde_json::Map;
use std::collections::HashMap;

/// 单条上游原始模型（合并前的中间结构）。
pub(crate) struct RawModel {
    pub id: String,
    pub name: String,
    pub entry: Value,
    pub thinking_mode: &'static str,
    pub upstream_thinking_level: Option<String>,
    pub raw_thinking_budget: Option<i64>,
    pub max_output_tokens: Option<i64>,
}

/// 请求路由解析结果：携带实际发往上游的模型 ID、目录条目、命中的变体与标准档位。
#[derive(Clone, Debug)]
pub struct ResolvedTarget {
    pub wire_model: String,
    pub entry: Option<Value>,
    pub variant: Option<Value>,
    pub level: Option<String>,
}

fn merge_quota(target: &mut Value, incoming: &Value) {
    if !incoming.is_object() {
        return;
    }
    if !target.is_object() {
        *target = incoming.clone();
        return;
    }
    match (util::quota_fraction(target), util::quota_fraction(incoming)) {
        (None, Some(_)) => *target = incoming.clone(),
        (Some(cur), Some(next)) if next + 1e-6 < cur => *target = incoming.clone(),
        _ => {}
    }
}

fn merge_entry_fields(target: &mut Value, incoming: &Value, is_image: bool) {
    if !is_image {
        for key in [
            "supportsToolCall",
            "supportsImages",
            "supportsReasoning",
            "supportsThinking",
            "isDefault",
        ] {
            if incoming[key] == true {
                target[key] = json!(true);
            }
        }
    } else if incoming["supportsImages"] == true {
        target["supportsImages"] = json!(true);
    }
    if incoming["visibility"].as_str() == Some("shown") {
        target["visibility"] = json!("shown");
    }
    for key in ["maxInputTokens", "maxOutputTokens"] {
        if let Some(next) = incoming[key].as_i64().filter(|n| *n > 0) {
            let cur = target[key].as_i64().unwrap_or(0);
            if next > cur {
                target[key] = json!(next);
            }
        }
    }
    if incoming["quota"].is_object() {
        let mut current = target.get("quota").cloned().unwrap_or(Value::Null);
        merge_quota(&mut current, &incoming["quota"]);
        target["quota"] = current;
    }
    if target.get("supportedMimeTypes").is_none()
        && (incoming["supportedMimeTypes"].is_array() || incoming["supportedMimeTypes"].is_object())
    {
        target["supportedMimeTypes"] = incoming["supportedMimeTypes"].clone();
    }
}

fn variant_record(raw: &RawModel, level: &str) -> Value {
    let mut record = json!({
        "id": raw.id,
        "thinkingMode": raw.thinking_mode,
    });
    if raw.thinking_mode == "level" {
        let wire_level = raw
            .upstream_thinking_level
            .clone()
            .unwrap_or_else(|| tiers::to_upstream_thinking_level(level).to_string());
        record["thinkingLevel"] = json!(wire_level);
    } else if let Some(budget) = raw.raw_thinking_budget {
        record["thinkingBudget"] = json!(budget);
    }
    if let Some(max_out) = raw.max_output_tokens {
        record["maxOutputTokens"] = json!(max_out);
    }
    record
}

/// 将同基础模型的不同思考等级条目合并为一条，声明 `reasoningLevels` 并保留变体路由表。
pub(crate) fn collapse_raw(items: Vec<RawModel>) -> Vec<Value> {
    let mut base_counts: HashMap<String, usize> = HashMap::new();
    for item in &items {
        let (base_id, _) = tiers::strip_effort_id(&item.id, true);
        *base_counts.entry(base_id.to_ascii_lowercase()).or_insert(0) += 1;
    }

    struct Group {
        id: String,
        name: String,
        has_bare: bool,
        bare_level_hint: Option<String>,
        thinking_mode: &'static str,
        entry: Value,
        variants: HashMap<String, Value>,
    }

    let mut groups: Vec<Group> = Vec::new();
    let mut by_base: HashMap<String, usize> = HashMap::new();

    for item in items {
        let (max_base, _) = tiers::strip_effort_id(&item.id, true);
        let (_, had_paren) = tiers::strip_parenthesized_effort(&item.name);
        let allow_max = had_paren
            || base_counts
                .get(&max_base.to_ascii_lowercase())
                .copied()
                .unwrap_or(0)
                > 1;
        let (base_id, level_opt) = tiers::strip_effort_id(&item.id, allow_max);
        let (base_name, _) = tiers::strip_effort_name(&item.name, allow_max || level_opt.is_some());
        let final_name = if item.name.eq_ignore_ascii_case(&item.id) {
            base_id.to_string()
        } else {
            base_name.to_string()
        };
        let base_key = base_id.to_ascii_lowercase();
        let is_img = catalog_shape::image(base_id);

        if let Some(&idx) = by_base.get(&base_key) {
            let group = &mut groups[idx];
            merge_entry_fields(&mut group.entry, &item.entry, is_img);
            if item.thinking_mode == "level" {
                group.thinking_mode = "level";
            }
            if let Some(level) = level_opt {
                group
                    .variants
                    .entry(level.to_string())
                    .or_insert_with(|| variant_record(&item, level));
            } else {
                group.has_bare = true;
                group.id = item.id.clone();
                if !final_name.is_empty() {
                    group.name = final_name;
                }
                if group.bare_level_hint.is_none() {
                    group.bare_level_hint = item.upstream_thinking_level.clone();
                }
            }
        } else {
            let idx = groups.len();
            by_base.insert(base_key, idx);
            let mut variants = HashMap::new();
            let mut has_bare = false;
            let mut bare_level_hint = None;
            if let Some(level) = level_opt {
                variants.insert(level.to_string(), variant_record(&item, level));
            } else {
                has_bare = true;
                bare_level_hint = item.upstream_thinking_level.clone();
            }
            groups.push(Group {
                id: base_id.to_string(),
                name: final_name,
                has_bare,
                bare_level_hint,
                thinking_mode: item.thinking_mode,
                entry: item.entry,
                variants,
            });
        }
    }

    groups
        .into_iter()
        .map(|mut group| {
            group.entry["id"] = json!(group.id);
            group.entry["name"] = json!(group.name);
            let supports_thinking = group.entry["supportsThinking"] == true;

            if !group.variants.is_empty() {
                let mut levels: Vec<String> = group.variants.keys().cloned().collect();
                tiers::sort_levels(&mut levels);
                let default_level =
                    tiers::pick_default_level(&levels, group.bare_level_hint.as_deref());
                let mut ordered_variants = Map::new();
                for level in &levels {
                    if let Some(meta) = group.variants.remove(level) {
                        ordered_variants.insert(level.clone(), meta);
                    }
                }
                group.entry["reasoningLevels"] = json!(levels);
                group.entry["reasoningSource"] = json!("upstream");
                group.entry["reasoningBindingSupported"] = json!(true);
                if let Some(default) = &default_level {
                    group.entry["reasoningDefaultLevel"] = json!(default);
                }
                if levels.len() == 1 && !group.has_bare {
                    group.entry["reasoningFixedLevel"] = json!(levels[0]);
                }
                group.entry["antigravityVariants"] = Value::Object(ordered_variants);
                group.entry["antigravityThinkingMode"] = json!(group.thinking_mode);

                if group.thinking_mode == "level" {
                    if let Some(obj) = group.entry.as_object_mut() {
                        obj.remove("thinkingBudget");
                    }
                } else if let Some(budget) = default_level
                    .as_deref()
                    .and_then(|lvl| {
                        group.entry["antigravityVariants"][lvl]["thinkingBudget"].as_i64()
                    })
                    .filter(|n| *n > 0)
                {
                    group.entry["thinkingBudget"] = json!(budget);
                }
            } else if supports_thinking {
                group.entry["antigravityThinkingMode"] = json!(group.thinking_mode);
                if group.thinking_mode == "level" {
                    if let Some(obj) = group.entry.as_object_mut() {
                        obj.remove("thinkingBudget");
                    }
                }
            }

            group.entry
        })
        .collect()
}

fn select_variant_from_entry(
    entry: &Value,
    effort: Option<&str>,
) -> Option<(String, Value, String)> {
    let variants = entry["antigravityVariants"].as_object()?;
    if variants.is_empty() {
        return None;
    }
    let mut levels: Vec<String> = entry["reasoningLevels"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|lvl| variants.contains_key(*lvl))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if levels.is_empty() {
        levels = variants.keys().cloned().collect();
        tiers::sort_levels(&mut levels);
    }
    let selected = effort
        .filter(|raw| !model_rules::reasoning_is_off(raw))
        .and_then(|raw| tiers::pick_closest_level(&levels, raw))
        .or_else(|| {
            entry["reasoningDefaultLevel"]
                .as_str()
                .filter(|lvl| variants.contains_key(*lvl))
                .map(str::to_string)
        })
        .or_else(|| tiers::pick_default_level(&levels, None))?;
    let variant = variants.get(&selected)?.clone();
    let wire = variant["id"].as_str()?.trim().to_string();
    (!wire.is_empty()).then_some((wire, variant, selected))
}

fn find_in_list(list: &[Value], requested: &str, effort: Option<&str>) -> Option<ResolvedTarget> {
    let trimmed = requested.trim();
    // 1. 精确匹配基础模型 ID（含已合并的思考等级变体）
    if let Some(row) = list
        .iter()
        .find(|row| row["id"].as_str().is_some_and(|id| id.eq_ignore_ascii_case(trimmed)))
    {
        if let Some((wire_model, variant, level)) = select_variant_from_entry(row, effort) {
            return Some(ResolvedTarget {
                wire_model,
                entry: Some(row.clone()),
                variant: Some(variant),
                level: Some(level),
            });
        }
        let wire_model = aliases::for_catalog(trimmed, list);
        return Some(ResolvedTarget {
            wire_model,
            entry: Some(row.clone()),
            variant: None,
            level: effort
                .filter(|raw| !model_rules::reasoning_is_off(raw))
                .and_then(tiers::normalize_level)
                .map(str::to_string),
        });
    }
    // 2. 兼容旧配置/客户端直接请求带思考等级后缀的上游具名变体
    for row in list {
        let Some(variants) = row["antigravityVariants"].as_object() else {
            continue;
        };
        for (level, variant) in variants {
            if variant["id"]
                .as_str()
                .is_some_and(|id| id.eq_ignore_ascii_case(trimmed))
            {
                let wire_model = variant["id"].as_str().unwrap_or(trimmed).to_string();
                return Some(ResolvedTarget {
                    wire_model,
                    entry: Some(row.clone()),
                    variant: Some(variant.clone()),
                    level: Some(level.clone()),
                });
            }
        }
    }
    None
}

/// 统一请求目标解析：优先按账号真实目录的变体表解析，次选内置兜底，最后回落旧别名规则。
pub fn resolve_target(list: &[Value], requested: &str, effort: Option<&str>) -> ResolvedTarget {
    let trimmed = requested.trim();
    if let Some(hit) = find_in_list(list, trimmed, effort) {
        return hit;
    }
    let fallback_list = catalog_shape::fallback();
    if let Some(mut hit) = find_in_list(&fallback_list, trimmed, effort) {
        // 兜底命中的 `gemini-3.1-pro-high` 若当前账号目录只提供 `gemini-pro-agent`，保留原有回落兼容。
        if hit.wire_model.eq_ignore_ascii_case("gemini-3.1-pro-high")
            && !list.iter().any(|row| {
                row["id"]
                    .as_str()
                    .is_some_and(|id| id.eq_ignore_ascii_case("gemini-3.1-pro"))
            })
            && list.iter().any(|row| {
                row["id"]
                    .as_str()
                    .is_some_and(|id| id.eq_ignore_ascii_case("gemini-pro-agent"))
            })
        {
            hit.wire_model = "gemini-pro-agent".to_string();
        }
        return hit;
    }
    if let Some(argon_wire) = argon::resolve_model(trimmed, effort) {
        let level = tiers::strip_effort_id(argon_wire, false)
            .1
            .map(str::to_string);
        return ResolvedTarget {
            wire_model: argon_wire.to_string(),
            entry: catalog_shape::entry(list, trimmed)
                .or_else(|| catalog_shape::entry(&fallback_list, trimmed)),
            variant: None,
            level,
        };
    }
    let wire_model = aliases::for_catalog(trimmed, list);
    let matched_entry = catalog_shape::entry(list, &wire_model)
        .or_else(|| catalog_shape::entry(&fallback_list, &wire_model));
    let level = tiers::strip_effort_id(&wire_model, false)
        .1
        .or_else(|| {
            effort
                .filter(|raw| !model_rules::reasoning_is_off(raw))
                .and_then(tiers::normalize_level)
        })
        .map(str::to_string);
    ResolvedTarget {
        wire_model,
        entry: matched_entry,
        variant: None,
        level,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog_shape::{collapse_cached_entries, entry, fallback, needs_collapse, parse};

    fn sample_upstream_body() -> Value {
        json!({
            "models": {
                "claude-opus-5-5-high": {
                    "displayName": "Claude Opus 5.5 (High)",
                    "supportsImages": true,
                    "supportsThinking": true,
                    "thinkingLevel": 3,
                    "maxTokens": 1000000,
                    "maxOutputTokens": 128000,
                    "recommended": true
                },
                "claude-opus-5-5-low": {
                    "displayName": "Claude Opus 5.5 (Low)",
                    "supportsImages": true,
                    "supportsThinking": true,
                    "thinkingLevel": 1,
                    "maxTokens": 1000000,
                    "maxOutputTokens": 128000
                },
                "claude-opus-5-5-medium": {
                    "displayName": "Claude Opus 5.5 (Medium)",
                    "supportsImages": true,
                    "supportsThinking": true,
                    "thinkingLevel": 2,
                    "maxTokens": 1000000,
                    "maxOutputTokens": 128000
                },
                "gemini-4-hydrogen-low": {
                    "displayName": "Gemini 4 Hydrogen (Low)",
                    "supportsImages": true,
                    "supportsThinking": true,
                    "maxTokens": 1048576,
                    "maxOutputTokens": 262144
                },
                "gemini-4-hydrogen-medium": {
                    "displayName": "Gemini 4 Hydrogen (Medium)",
                    "supportsImages": true,
                    "supportsThinking": true,
                    "maxTokens": 1048576,
                    "maxOutputTokens": 262144
                },
                "gemini-4-hydrogen-high": {
                    "displayName": "Gemini 4 Hydrogen (High)",
                    "supportsImages": true,
                    "supportsThinking": true,
                    "maxTokens": 1048576,
                    "maxOutputTokens": 262144
                },
                "gemini-4-hydrogen-xhigh": {
                    "displayName": "Gemini 4 Hydrogen (XHigh)",
                    "supportsImages": true,
                    "supportsThinking": true,
                    "maxTokens": 1048576,
                    "maxOutputTokens": 262144
                },
                "gemini-4-hydrogen-tiered-high": {
                    "displayName": "Gemini 4 Hydrogen Tiered (High)",
                    "supportsImages": true,
                    "supportsThinking": true,
                    "maxTokens": 1048576,
                    "maxOutputTokens": 262144
                },
                "gemini-4-hydrogen-tiered-xhigh": {
                    "displayName": "Gemini 4 Hydrogen Tiered (XHigh)",
                    "supportsImages": true,
                    "supportsThinking": true,
                    "maxTokens": 1048576,
                    "maxOutputTokens": 262144
                },
                "gemini-4-hydrogen-sonar": {
                    "displayName": "Gemini 4 Hydrogen Sonar",
                    "supportsImages": true,
                    "supportsThinking": true,
                    "maxTokens": 1048576,
                    "maxOutputTokens": 65536
                },
                "gemini-3-flash": {
                    "displayName": "Gemini 3 Flash",
                    "supportsImages": true,
                    "supportsThinking": true,
                    "maxTokens": 1048576,
                    "maxOutputTokens": 65536
                },
                "gpt-oss-120b-medium": {
                    "displayName": "GPT-OSS 120B (Medium)",
                    "supportsThinking": true,
                    "thinkingBudget": 8192
                }
            }
        })
    }

    #[test]
    fn parse_collapses_effort_variants_and_keeps_tiered_and_sonar_separate() {
        let models = parse(&sample_upstream_body());
        let ids: Vec<&str> = models.iter().filter_map(|m| m["id"].as_str()).collect();
        assert_eq!(ids.len(), 6, "unexpected merged model list: {ids:?}");
        for expected in [
            "claude-opus-5-5",
            "gemini-4-hydrogen",
            "gemini-4-hydrogen-tiered",
            "gemini-4-hydrogen-sonar",
            "gemini-3-flash",
            "gpt-oss-120b",
        ] {
            assert!(ids.contains(&expected), "missing {expected} in {ids:?}");
        }

        let opus = entry(&models, "claude-opus-5-5").unwrap();
        assert_eq!(opus["name"], "Claude Opus 5.5");
        assert_eq!(opus["reasoningLevels"], json!(["low", "medium", "high"]));
        assert_eq!(opus["reasoningDefaultLevel"], "medium");
        assert!(opus.get("reasoningFixedLevel").is_none());
        assert_eq!(opus["antigravityThinkingMode"], "level");
        assert!(opus.get("thinkingBudget").is_none());

        let hydrogen = entry(&models, "gemini-4-hydrogen").unwrap();
        assert_eq!(
            hydrogen["reasoningLevels"],
            json!(["low", "medium", "high", "xhigh"])
        );
        assert_eq!(hydrogen["reasoningDefaultLevel"], "medium");

        let tiered = entry(&models, "gemini-4-hydrogen-tiered").unwrap();
        assert_eq!(tiered["name"], "Gemini 4 Hydrogen Tiered");
        assert_eq!(tiered["reasoningLevels"], json!(["high", "xhigh"]));
        assert_eq!(tiered["reasoningDefaultLevel"], "high");

        let sonar = entry(&models, "gemini-4-hydrogen-sonar").unwrap();
        assert!(sonar.get("reasoningLevels").is_none());

        let gpt = entry(&models, "gpt-oss-120b").unwrap();
        assert_eq!(gpt["reasoningLevels"], json!(["medium"]));
        assert_eq!(gpt["reasoningFixedLevel"], "medium");
        assert_eq!(gpt["antigravityThinkingMode"], "budget");
    }

    #[test]
    fn resolve_target_routes_by_effort_and_preserves_legacy_variant_ids() {
        let models = parse(&sample_upstream_body());

        let opus_default = resolve_target(&models, "claude-opus-5-5", None);
        assert_eq!(opus_default.wire_model, "claude-opus-5-5-medium");
        assert_eq!(opus_default.level.as_deref(), Some("medium"));

        let opus_high = resolve_target(&models, "claude-opus-5-5", Some("high"));
        assert_eq!(opus_high.wire_model, "claude-opus-5-5-high");

        let opus_xhigh = resolve_target(&models, "claude-opus-5-5", Some("xhigh"));
        assert_eq!(opus_xhigh.wire_model, "claude-opus-5-5-high");

        let tiered_xhigh = resolve_target(&models, "gemini-4-hydrogen-tiered", Some("xhigh"));
        assert_eq!(tiered_xhigh.wire_model, "gemini-4-hydrogen-tiered-xhigh");

        let tiered_default = resolve_target(&models, "gemini-4-hydrogen-tiered", None);
        assert_eq!(tiered_default.wire_model, "gemini-4-hydrogen-tiered-high");

        // 兼容旧配置直接传带后缀变体 ID
        let legacy = resolve_target(&models, "claude-opus-5-5-high", None);
        assert_eq!(legacy.wire_model, "claude-opus-5-5-high");
        assert_eq!(legacy.level.as_deref(), Some("high"));

        let gpt = resolve_target(&models, "gpt-oss-120b", None);
        assert_eq!(gpt.wire_model, "gpt-oss-120b-medium");
    }

    #[test]
    fn fallback_and_legacy_cache_entries_are_collapsed() {
        let defaults = fallback();
        assert!(
            !needs_collapse(&defaults),
            "fallback must not expose unmerged effort rows"
        );
        let pro = entry(&defaults, "gemini-3.1-pro").expect("gemini-3.1-pro in fallback");
        assert_eq!(pro["reasoningLevels"], json!(["low", "high"]));
        assert_eq!(pro["reasoningDefaultLevel"], "high");

        let legacy_cached = vec![
            json!({ "id": "claude-opus-5-5-high", "name": "Claude Opus 5.5 (High)", "supportsThinking": true, "maxOutputTokens": 128000 }),
            json!({ "id": "claude-opus-5-5-low", "name": "Claude Opus 5.5 (Low)", "supportsThinking": true, "maxOutputTokens": 128000 }),
        ];
        assert!(needs_collapse(&legacy_cached));
        let upgraded = collapse_cached_entries(legacy_cached);
        assert_eq!(upgraded.len(), 1);
        assert_eq!(upgraded[0]["id"], "claude-opus-5-5");
        assert_eq!(upgraded[0]["reasoningLevels"], json!(["low", "high"]));
        assert!(!needs_collapse(&upgraded));
    }
}
