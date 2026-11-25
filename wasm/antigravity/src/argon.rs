//! Argon 仅以同账号目录或专属额度桶为证据，绝不根据 Pro/Ultra 名称猜资格。
//! 迁移原生 argon.rs；额度最紧窗口和 LOW/MEDIUM/HIGH 参数保持原语义。

use super::{catalog_shape, model_rules};
use agent2api_wasm_sdk::{json, Value};

const BASE: &str = "gemini-4-argon";
const VARIANTS: [(&str, &str); 3] = [
    ("gemini-4-argon-low", "Gemini 4 Argon (Low)"),
    ("gemini-4-argon-medium", "Gemini 4 Argon (Medium)"),
    ("gemini-4-argon-high", "Gemini 4 Argon (High)"),
];
fn tier(id: &str) -> Option<usize> {
    if id.trim().eq_ignore_ascii_case(BASE) {
        return Some(1);
    }
    VARIANTS
        .iter()
        .position(|(known, _)| id.trim().eq_ignore_ascii_case(known))
}
pub fn is_model(id: &str) -> bool {
    tier(id).is_some()
}
pub fn resolve_model(id: &str, effort: Option<&str>) -> Option<&'static str> {
    let default = tier(id)?;
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
pub fn is_quota_bucket(group: &Value, bucket: &Value) -> bool {
    dedicated_token(group, bucket) == Some("argon")
}
pub fn dedicated_token(group: &Value, bucket: &Value) -> Option<&'static str> {
    let texts = [
        group.get("displayName"),
        group.get("display_name"),
        bucket.get("bucketId"),
        bucket.get("bucket_id"),
        bucket.get("displayName"),
        bucket.get("display_name"),
    ];
    for token in ["argon", "carbon", "hydrogen"] {
        if texts
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|v| v.to_ascii_lowercase().contains(token))
        {
            return Some(token);
        }
    }
    None
}
pub fn has_dedicated_quota(summary: &Value) -> bool {
    summary["groups"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|group| {
            group["buckets"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|bucket| is_quota_bucket(group, bucket))
        })
}
fn quota(summary: &Value) -> Value {
    let tightest = summary["groups"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|group| {
            group["buckets"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(move |bucket| is_quota_bucket(group, bucket))
        })
        .filter_map(|bucket| super::util::bucket_fraction(bucket).map(|fraction| (fraction, bucket)))
        .min_by(|(a, _), (b, _)| a.total_cmp(b));
    let Some((fraction, bucket)) = tightest else {
        return Value::Null;
    };
    let mut value = json!({ "remainingFraction": fraction });
    if let Some(reset) = bucket["resetTime"].as_str().filter(|s| !s.is_empty()) {
        value["resetTime"] = json!(reset);
    }
    value
}
pub fn supplement(models: &mut Vec<Value>, summary: Option<&Value>) {
    if models
        .iter()
        .any(|m| m["id"].as_str().is_some_and(is_model))
    {
        return;
    }
    if let Some(summary) = summary.filter(|s| has_dedicated_quota(s)) {
        let quota = quota(summary);
        let pairs: Vec<(&str, Value)> = VARIANTS
            .iter()
            .enumerate()
            .map(|(index, (id, name))| {
                (
                    *id,
                    json!({
                        "displayName": name,
                        "supportsImages": true,
                        "supportsThinking": true,
                        "thinkingLevel": (index + 1) as i64,
                        "maxTokens": 1_048_576,
                        "maxOutputTokens": 65_536,
                        "quotaInfo": quota,
                    }),
                )
            })
            .collect();
        models.extend(catalog_shape::collapse_pairs(&pairs));
    }
}

pub fn quota_models(
    models: &[Value],
    summary: &Value,
    group: &Value,
    bucket: &Value,
) -> Vec<String> {
    let token = dedicated_token(group, bucket);
    let dedicated_tokens: Vec<&'static str> = summary["groups"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|g| {
            g["buckets"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(move |b| dedicated_token(g, b))
        })
        .collect();
    let name = super::util::text(group, &["displayName", "display_name"]).to_ascii_lowercase();
    let bucket_id = super::util::text(bucket, &["bucketId", "bucket_id"]).to_ascii_lowercase();
    let third = name.contains("claude") || name.contains("gpt") || bucket_id.starts_with("3p-");
    let gemini = !third && (name.contains("gemini") || bucket_id.starts_with("gemini-"));
    let mut ids: Vec<String> = models
        .iter()
        .filter_map(|model| {
            let id = model["id"].as_str()?;
            let lower = id.to_ascii_lowercase();
            let matches = if let Some(tok) = token {
                if tok == "argon" {
                    is_model(id)
                } else {
                    lower.contains(tok)
                }
            } else if third {
                lower.starts_with("claude-") || lower.starts_with("gpt-oss-")
            } else {
                gemini
                    && lower.starts_with("gemini-")
                    && !dedicated_tokens.iter().any(|tok| {
                        if *tok == "argon" {
                            is_model(id)
                        } else {
                            lower.contains(*tok)
                        }
                    })
            };
            matches.then(|| id.to_string())
        })
        .collect();
    let separate_argon = dedicated_tokens.contains(&"argon");
    if token == Some("argon") || gemini && !separate_argon {
        // 额度范围说明不是新增模型广告；余额可以先于目录加载。
        if !ids.iter().any(|known| known.eq_ignore_ascii_case(BASE)) {
            ids.push(BASE.to_string());
        }
    }
    ids
}
