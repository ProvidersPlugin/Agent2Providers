//! 逐模型配额明细解析：融合 fetchAvailableModels 单模型 quotaInfo 与 retrieveUserQuotaSummary 分组窗口。

use super::{argon, catalog_shape, tiers, usage_windows, util};
use agent2api_wasm_sdk::{json, Value};
use std::collections::{HashMap, HashSet};

#[derive(Clone)]
pub struct BucketReading {
    pub window: String,
    pub fraction: f64,
    pub reset_at: Option<i64>,
}

#[derive(Clone)]
pub struct GroupReading {
    pub name: String,
    pub dedicated: Option<&'static str>,
    pub third_party: bool,
    pub bucket_5h: Option<BucketReading>,
    pub bucket_weekly: Option<BucketReading>,
    pub first_bucket: Option<BucketReading>,
}

struct OfficialSupport {
    ids: HashSet<String>,
    rank: HashMap<String, usize>,
}

pub fn collect_groups(summary: Option<&Value>) -> Vec<GroupReading> {
    let mut out = Vec::new();
    let Some(groups) = summary.and_then(|s| s["groups"].as_array()) else {
        return out;
    };
    for group in groups {
        let name = usage_windows::group_name(&util::text(group, &["displayName", "display_name"]));
        let lower_name = name.to_ascii_lowercase();
        let mut dedicated = None;
        let mut bucket_5h = None;
        let mut bucket_weekly = None;
        let mut first_bucket = None;

        for bucket in group["buckets"].as_array().into_iter().flatten() {
            if dedicated.is_none() {
                dedicated = argon::dedicated_token(group, bucket);
            }
            let Some(fraction) = util::bucket_fraction(bucket) else {
                continue;
            };
            if !fraction.is_finite() {
                continue;
            }
            let reset = util::rfc3339(&util::text(bucket, &["resetTime", "reset_time"]));
            let window = usage_windows::window_code(bucket);
            let reading = BucketReading {
                window: window.clone(),
                fraction,
                reset_at: reset,
            };
            if first_bucket.is_none() {
                first_bucket = Some(reading.clone());
            }
            if window == "5h" && bucket_5h.is_none() {
                bucket_5h = Some(reading);
            } else if window == "weekly" && bucket_weekly.is_none() {
                bucket_weekly = Some(reading);
            }
        }

        let third_party = dedicated.is_none()
            && (lower_name.contains("claude")
                || lower_name.contains("gpt")
                || lower_name.contains("3p")
                || group["buckets"].as_array().into_iter().flatten().any(|b| {
                    util::text(b, &["bucketId", "bucket_id"])
                        .to_ascii_lowercase()
                        .starts_with("3p-")
                }));
        out.push(GroupReading {
            name,
            dedicated,
            third_party,
            bucket_5h,
            bucket_weekly,
            first_bucket,
        });
    }
    out
}

fn bottleneck(group: &GroupReading) -> Option<BucketReading> {
    match (&group.bucket_5h, &group.bucket_weekly) {
        (Some(h), Some(w)) => {
            if w.fraction <= 0.001 || w.fraction < h.fraction {
                Some(w.clone())
            } else if h.fraction < w.fraction {
                Some(h.clone())
            } else if w.reset_at.is_some() {
                Some(w.clone())
            } else {
                Some(h.clone())
            }
        }
        (Some(h), None) => Some(h.clone()),
        (None, Some(w)) => Some(w.clone()),
        (None, None) => group.first_bucket.clone(),
    }
}

fn match_group<'a>(id_lower: &str, groups: &'a [GroupReading]) -> Option<&'a GroupReading> {
    let model_token = ["argon", "carbon", "hydrogen"]
        .into_iter()
        .find(|tok| id_lower.contains(tok));
    if let Some(tok) = model_token {
        if let Some(dedicated) = groups.iter().find(|g| g.dedicated == Some(tok)) {
            return Some(dedicated);
        }
    }
    let third = model_token.is_none()
        && (id_lower.starts_with("claude") || id_lower.starts_with("gpt"));
    if third {
        return groups.iter().find(|g| g.third_party && g.dedicated.is_none());
    }
    groups
        .iter()
        .find(|g| {
            !g.third_party
                && g.dedicated.is_none()
                && (g.name.to_ascii_lowercase().contains("gemini") || !g.name.is_empty())
        })
        .or_else(|| groups.iter().find(|g| !g.third_party && g.dedicated.is_none()))
}

fn official_support(body: &Value) -> OfficialSupport {
    let mut ids = HashSet::new();
    let mut rank = HashMap::new();
    let sorts = body
        .get("agentModelSorts")
        .or_else(|| body.get("agent_model_sorts"))
        .and_then(Value::as_array);
    for sort in sorts.into_iter().flatten() {
        for group in sort["groups"].as_array().into_iter().flatten() {
            let model_ids = group
                .get("modelIds")
                .or_else(|| group.get("model_ids"))
                .and_then(Value::as_array);
            for raw_id in model_ids.into_iter().flatten() {
                if let Some(id) = raw_id.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    let key = id.to_ascii_lowercase();
                    ids.insert(key.clone());
                    let next = rank.len() + 1;
                    rank.entry(key).or_insert(next);
                }
            }
        }
    }
    let image_ids = body
        .get("imageGenerationModelIds")
        .or_else(|| body.get("image_generation_model_ids"))
        .and_then(Value::as_array);
    for raw_id in image_ids.into_iter().flatten() {
        if let Some(id) = raw_id.as_str().map(str::trim).filter(|s| !s.is_empty()) {
            ids.insert(id.to_ascii_lowercase());
        }
    }
    let tiered_map = body
        .get("tieredModelIds")
        .or_else(|| body.get("tiered_model_ids"))
        .and_then(Value::as_object);
    for value in tiered_map.into_iter().flat_map(|m| m.values()) {
        for raw_id in value.as_array().into_iter().flatten() {
            if let Some(id) = raw_id.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                ids.insert(id.to_ascii_lowercase());
            }
        }
    }
    if !ids.is_empty() {
        ids.insert("gemini-3.1-flash-lite".to_string());
    }
    OfficialSupport { ids, rank }
}

fn iter_raw_models(body: &Value) -> Vec<(String, &Value)> {
    match &body["models"] {
        Value::Object(map) => map.iter().map(|(id, info)| (id.clone(), info)).collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(|info| Some((info["id"].as_str()?.to_string(), info)))
            .collect(),
        _ => Vec::new(),
    }
}

fn fallback_group_name(id_lower: &str) -> &'static str {
    if id_lower.contains("argon") {
        "Gemini 4 Argon"
    } else if id_lower.starts_with("claude") || id_lower.starts_with("gpt") {
        "Claude/GPT"
    } else {
        "Gemini"
    }
}

fn category_rank(id_lower: &str) -> u8 {
    if id_lower.starts_with("gemini-4") {
        0
    } else if id_lower.starts_with("gemini-3.8") || id_lower.starts_with("gemini-3.9") {
        1
    } else if id_lower.starts_with("gemini-3.1-pro") {
        2
    } else if id_lower.contains("image") {
        3
    } else if id_lower.starts_with("gemini-") {
        4
    } else if id_lower.starts_with("claude-") && id_lower.contains("-5-") {
        5
    } else if id_lower.starts_with("claude-") {
        6
    } else {
        7
    }
}

fn round_percent(fraction: f64) -> f64 {
    (fraction.clamp(0.0, 1.0) * 10_000.0).round() / 100.0
}

fn format_view(percent: Option<f64>) -> String {
    match percent {
        Some(value) => format!("剩 {}%", usage_windows::percent_text(value)),
        None => "—".into(),
    }
}

fn build_row(
    id: &str,
    name: &str,
    group_name: &str,
    model_frac: Option<f64>,
    model_reset: Option<i64>,
    group_bucket: Option<BucketReading>,
    info: Option<&Value>,
) -> Value {
    let group_frac = group_bucket.as_ref().map(|b| b.fraction);
    let group_reset = group_bucket.as_ref().and_then(|b| b.reset_at);
    let group_win = group_bucket.as_ref().map(|b| b.window.as_str()).unwrap_or("");
    let (final_frac, final_reset, window) = match (model_frac, group_frac) {
        (Some(m), Some(g)) => {
            if m <= 0.0 {
                (Some(0.0), model_reset.or(group_reset), String::new())
            } else if g < m {
                (Some(g), group_reset.or(model_reset), group_win.to_string())
            } else {
                (Some(m), model_reset.or(group_reset), if group_win == "5h" { "5h".to_string() } else { String::new() })
            }
        }
        (Some(m), None) => (Some(m), model_reset, String::new()),
        (None, Some(g)) => (Some(g), group_reset, group_win.to_string()),
        (None, None) => (None, None, String::new()),
    };
    let remaining_pct = final_frac.map(round_percent);
    let mut row = json!({
        "id": id,
        "name": name,
        "group": group_name,
        "window": window,
        "remainingPercent": remaining_pct,
        "view": format_view(remaining_pct),
    });
    if let Some(m) = model_frac {
        row["modelPercent"] = json!(round_percent(m));
    }
    if let Some(g) = group_frac {
        row["groupPercent"] = json!(round_percent(g));
    }
    if let Some(at) = final_reset.filter(|_| final_frac.is_some_and(|f| f < 1.0)) {
        row["resetsAt"] = json!(at);
    }
    if let Some(info) = info {
        if info["recommended"].as_bool() == Some(true) {
            row["recommended"] = json!(true);
        }
        if let Some(b) = info.get("thinkingBudget").or_else(|| info.get("thinking_budget")).and_then(Value::as_i64) {
            row["thinkingBudget"] = json!(b);
        }
        if info.get("supportsThinking").or_else(|| info.get("supports_thinking")).and_then(Value::as_bool) == Some(true) {
            row["supportsThinking"] = json!(true);
        }
        if info.get("supportsImages").or_else(|| info.get("supports_images")).and_then(Value::as_bool) == Some(true) {
            row["supportsImages"] = json!(true);
        }
    }
    row
}

fn row_percent(row: &Value) -> Option<f64> {
    row["remainingPercent"].as_f64().filter(|v| v.is_finite())
}

fn prefer_incoming_reading(existing: &Value, incoming: &Value) -> bool {
    match (row_percent(existing), row_percent(incoming)) {
        (None, Some(_)) => true,
        (Some(_), None) => false,
        (Some(e), Some(i)) if i + 1e-6 < e => true,
        (Some(e), Some(i)) if (i - e).abs() > 1e-6 => false,
        _ => {
            let ew = !existing["window"].as_str().unwrap_or("").trim().is_empty();
            let iw = !incoming["window"].as_str().unwrap_or("").trim().is_empty();
            if !ew && iw {
                return true;
            }
            if ew && !iw {
                return false;
            }
            match (existing["resetsAt"].as_i64(), incoming["resetsAt"].as_i64()) {
                (None, Some(_)) => true,
                (Some(er), Some(ir)) => ir < er,
                _ => false,
            }
        }
    }
}

fn merge_quota_row(existing: &mut Value, incoming: Value, adopt_identity: bool) {
    if adopt_identity {
        existing["id"] = incoming["id"].clone();
        existing["name"] = incoming["name"].clone();
        if !incoming["group"].as_str().unwrap_or("").is_empty() {
            existing["group"] = incoming["group"].clone();
        }
    }
    if prefer_incoming_reading(existing, &incoming) {
        existing["remainingPercent"] = incoming["remainingPercent"].clone();
        existing["view"] = incoming["view"].clone();
        existing["window"] = incoming["window"].clone();
        for key in ["modelPercent", "groupPercent", "resetsAt"] {
            if incoming.get(key).is_some_and(|v| !v.is_null()) {
                existing[key] = incoming[key].clone();
            } else if let Some(obj) = existing.as_object_mut() {
                obj.remove(key);
            }
        }
    } else {
        if existing["window"].as_str().unwrap_or("").trim().is_empty()
            && !incoming["window"].as_str().unwrap_or("").trim().is_empty()
        {
            existing["window"] = incoming["window"].clone();
        }
        if existing["resetsAt"].as_i64().is_none() && incoming["resetsAt"].as_i64().is_some() {
            existing["resetsAt"] = incoming["resetsAt"].clone();
        }
    }
    for flag in ["recommended", "supportsThinking", "supportsImages"] {
        if incoming[flag].as_bool() == Some(true) {
            existing[flag] = json!(true);
        }
    }
    if existing.get("thinkingBudget").is_none()
        && incoming.get("thinkingBudget").is_some_and(|v| !v.is_null())
    {
        existing["thinkingBudget"] = incoming["thinkingBudget"].clone();
    }
}

pub fn build(models_body: Option<&Value>, groups: &[GroupReading]) -> Vec<Value> {
    struct Candidate {
        rank: usize,
        id: String,
        name: String,
        row: Value,
    }
    let mut candidates: Vec<Candidate> = Vec::new();
    if let Some(body) = models_body {
        let support = official_support(body);
        let raw_items = iter_raw_models(body);
        let has_modern_flash = raw_items.iter().any(|(id, info)| {
            let lower = id.trim().to_ascii_lowercase();
            !catalog_shape::is_disabled(id, info)
                && (lower.starts_with("gemini-3.8")
                    || lower.starts_with("gemini-3.9")
                    || lower.starts_with("gemini-4"))
        });

        for (raw_id, info) in raw_items {
            let id = raw_id.trim();
            let lower = id.to_ascii_lowercase();
            let in_sort = support.ids.is_empty() || support.ids.contains(&lower);
            if !in_sort
                || !catalog_shape::is_quota_baseline_model(id, info, in_sort, !has_modern_flash)
            {
                continue;
            }
            let quota_info = info.get("quotaInfo").or_else(|| info.get("quota_info"));
            let model_frac = quota_info.and_then(util::quota_fraction);
            let model_reset = quota_info
                .map(|q| util::text(q, &["resetTime", "reset_time"]))
                .and_then(|s| util::rfc3339(&s));
            let matched = match_group(&lower, groups);
            let group_name = matched
                .map(|g| g.name.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| fallback_group_name(&lower));
            let group_bucket = matched.and_then(bottleneck);
            let display_name = util::text(info, &["displayName", "display_name"]);
            let label = if display_name.is_empty() {
                id.to_string()
            } else {
                display_name
            };
            let rank = support.rank.get(&lower).copied().unwrap_or(usize::MAX);
            candidates.push(Candidate {
                rank,
                id: id.to_string(),
                name: label.clone(),
                row: build_row(id, &label, group_name, model_frac, model_reset, group_bucket, Some(info)),
            });
        }
    }

    if let Some(argon_group) = groups.iter().find(|g| g.dedicated == Some("argon")) {
        let has_argon = candidates
            .iter()
            .any(|item| item.id.to_ascii_lowercase().contains("argon"));
        if !has_argon {
            let bucket = bottleneck(argon_group);
            let group_name = if argon_group.name.is_empty() {
                "Gemini 4 Argon"
            } else {
                argon_group.name.as_str()
            };
            let info = json!({ "recommended": true, "supportsThinking": true, "supportsImages": true });
            candidates.push(Candidate {
                rank: 0,
                id: "gemini-4-argon".to_string(),
                name: "Gemini 4 Argon".to_string(),
                row: build_row("gemini-4-argon", "Gemini 4 Argon", group_name, None, None, bucket, Some(&info)),
            });
        }
    }

    // 同一模型不同思考等级在配额展示里合并为一条基础模型；`-max` 仅在括号标注或存在同族兄弟时视作档位。
    let mut base_counts: HashMap<String, usize> = HashMap::new();
    for item in &candidates {
        let (base_id, _) = tiers::strip_effort_id(&item.id, true);
        *base_counts.entry(base_id.to_ascii_lowercase()).or_insert(0) += 1;
    }

    struct Collapsed {
        rank: usize,
        base_lower: String,
        bare_priority: u8,
        row: Value,
    }
    let mut collapsed: Vec<Collapsed> = Vec::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    let mut by_label: HashMap<String, usize> = HashMap::new();

    for mut item in candidates {
        let (max_id_base, _) = tiers::strip_effort_id(&item.id, true);
        let (_, had_paren) = tiers::strip_parenthesized_effort(&item.name);
        let allow_max = had_paren || base_counts.get(&max_id_base.to_ascii_lowercase()).copied().unwrap_or(0) > 1;
        let (base_id, level_opt) = tiers::strip_effort_id(&item.id, allow_max);
        let id_stripped = level_opt.is_some();
        let (base_name, _) = tiers::strip_effort_name(&item.name, allow_max || id_stripped);
        let final_name = if item.name.eq_ignore_ascii_case(&item.id) {
            base_id.to_string()
        } else {
            base_name.to_string()
        };
        item.row["id"] = json!(base_id);
        item.row["name"] = json!(final_name);

        let base_lower = base_id.to_ascii_lowercase();
        let label_key = final_name.trim().to_ascii_lowercase();
        let bare_priority = u8::from(id_stripped);
        let existing_idx = by_id
            .get(&base_lower)
            .copied()
            .or_else(|| (!label_key.is_empty()).then(|| by_label.get(&label_key).copied()).flatten());

        if let Some(idx) = existing_idx {
            let slot = &mut collapsed[idx];
            slot.rank = slot.rank.min(item.rank);
            let adopt_identity = bare_priority < slot.bare_priority;
            if adopt_identity {
                slot.bare_priority = bare_priority;
                slot.base_lower = base_lower.clone();
            }
            merge_quota_row(&mut slot.row, item.row, adopt_identity);
            by_id.entry(base_lower).or_insert(idx);
            if !label_key.is_empty() {
                by_label.entry(label_key).or_insert(idx);
            }
        } else {
            let idx = collapsed.len();
            by_id.insert(base_lower.clone(), idx);
            if !label_key.is_empty() {
                by_label.insert(label_key, idx);
            }
            collapsed.push(Collapsed {
                rank: item.rank,
                base_lower,
                bare_priority,
                row: item.row,
            });
        }
    }

    collapsed.sort_by(|a, b| {
        a.rank
            .cmp(&b.rank)
            .then_with(|| category_rank(&a.base_lower).cmp(&category_rank(&b.base_lower)))
            .then_with(|| a.base_lower.cmp(&b.base_lower))
    });

    collapsed.into_iter().map(|entry| entry.row).collect()
}
