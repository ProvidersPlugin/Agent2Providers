use serde_json::{json, Map, Value};

use crate::model_rules;

const BASE_ID: &str = "gemini-4-argon";
const VARIANTS: [(&str, &str); 3] = [
    ("gemini-4-argon-low", "Gemini 4 Argon (Low)"),
    ("gemini-4-argon-medium", "Gemini 4 Argon (Medium)"),
    ("gemini-4-argon-high", "Gemini 4 Argon (High)"),
];

pub(super) fn is_model(id: &str) -> bool {
    default_tier(id).is_some()
}

fn default_tier(id: &str) -> Option<usize> {
    let id = id.trim();
    if id.eq_ignore_ascii_case(BASE_ID) {
        return Some(2);
    }
    VARIANTS
        .iter()
        .position(|(known, _)| id.eq_ignore_ascii_case(known))
}

pub(super) fn resolve_model(id: &str, effort: Option<&str>) -> Option<&'static str> {
    let default = default_tier(id)?;
    let tier = effort
        .and_then(model_rules::reasoning_rank)
        .map(|rank| match rank {
            0 | 1 => 0,
            2 => 1,
            _ => 2,
        })
        .unwrap_or(default);
    VARIANTS.get(tier).map(|(id, _)| *id)
}

pub(super) fn thinking_config(chat: &Value, model: &str) -> Option<Value> {
    let effort = model_rules::read_client_level(chat);
    if effort.as_deref().is_some_and(model_rules::reasoning_is_off) {
        return None;
    }
    let model = resolve_model(model, effort.as_deref())?;
    let level = match default_tier(model)? {
        0 => "LOW",
        1 => "MEDIUM",
        _ => "HIGH",
    };
    // Argon 使用等级而非数字预算；低档不能降成不受支持的 MINIMAL。
    Some(json!({ "includeThoughts": true, "thinkingLevel": level }))
}

pub(super) fn is_quota_bucket(group: &Value, bucket: &Value) -> bool {
    [
        group.get("displayName"),
        bucket.get("bucketId"),
        bucket.get("displayName"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .any(|text| text.to_ascii_lowercase().contains("argon"))
}

pub(super) fn has_dedicated_quota(summary: &Value) -> bool {
    summary
        .get("groups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|group| {
            group
                .get("buckets")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|bucket| is_quota_bucket(group, bucket))
        })
}

fn quota_info(summary: &Value) -> Value {
    let tightest = summary
        .get("groups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|group| {
            group
                .get("buckets")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(move |bucket| is_quota_bucket(group, bucket))
        })
        .filter_map(|bucket| {
            let fraction = bucket.get("remainingFraction")?.as_f64()?;
            (fraction.is_finite() && (0.0..=1.0).contains(&fraction)).then_some((fraction, bucket))
        })
        .min_by(|(left, _), (right, _)| left.total_cmp(right));
    let Some((fraction, bucket)) = tightest else {
        return Value::Null;
    };
    let mut quota = Map::new();
    quota.insert("remainingFraction".to_string(), json!(fraction));
    if let Some(reset) = bucket
        .get("resetTime")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        quota.insert("resetTime".to_string(), json!(reset));
    }
    Value::Object(quota)
}

pub(super) fn supplement(models: &mut Vec<Value>, summary: Option<&Value>) {
    let listed = models.iter().any(|model| {
        model
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(is_model)
    });
    // 只接受同一账号的专属额度证据；不从 Pro 标签或普通 Gemini 额度推断权限。
    if !listed {
        if let Some(summary) = summary.filter(|value| has_dedicated_quota(value)) {
            let quota = quota_info(summary);
            for (id, name) in VARIANTS {
                let info = json!({
                    "displayName": name,
                    "supportsImages": true,
                    "supportsThinking": true,
                    "maxTokens": 1_048_576,
                    "maxOutputTokens": 65_536,
                    "quotaInfo": quota,
                });
                if let Some(model) = super::models::map_entry(id, &info) {
                    models.push(model);
                }
            }
        }
    }
    if models
        .iter()
        .any(|model| model.get("id").and_then(Value::as_str) == Some(BASE_ID))
    {
        return;
    }
    if let Some(mut alias) = models
        .iter()
        .find(|model| model.get("id").and_then(Value::as_str) == Some("gemini-4-argon-high"))
        .cloned()
    {
        alias["id"] = json!(BASE_ID);
        alias["name"] = json!("Gemini 4 Argon");
        models.push(alias);
    }
}

pub(super) fn quota_models(summary: &Value, group: &Value, bucket: &Value) -> Vec<String> {
    let dedicated = is_quota_bucket(group, bucket);
    let separate_argon = has_dedicated_quota(summary);
    let name = group
        .get("displayName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let bucket_id = bucket
        .get("bucketId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let third_party =
        name.contains("claude") || name.contains("gpt") || bucket_id.starts_with("3p-");
    let gemini = !third_party && (name.contains("gemini") || bucket_id.starts_with("gemini-"));
    let mut ids: Vec<String> = super::models::list()
        .iter()
        .filter_map(|model| {
            let id = model.get("id")?.as_str()?;
            let lower = id.to_ascii_lowercase();
            let matches = if dedicated {
                is_model(id)
            } else if third_party {
                lower.starts_with("claude-") || lower.starts_with("gpt-oss-")
            } else {
                gemini && lower.starts_with("gemini-") && !(separate_argon && is_model(id))
            };
            matches.then(|| id.to_string())
        })
        .collect();
    if dedicated || (gemini && !separate_argon) {
        // 余额可能先于目录刷新；明确模型范围不等于声明该账号已获得模型权限。
        for id in std::iter::once(BASE_ID).chain(VARIANTS.iter().map(|(id, _)| *id)) {
            if !ids.iter().any(|known| known == id) {
                ids.push(id.to_string());
            }
        }
    }
    ids
}
