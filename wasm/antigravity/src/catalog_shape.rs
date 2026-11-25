//! 原生目录的模型范围、兜底清单与原始条目解析。
//! 思考等级的合并折叠与变体选路委托给 `catalog_variants`。

use super::{aliases, argon, catalog_variants, tiers, util};
use agent2api_wasm_sdk::{json, Value};

pub use catalog_variants::resolve_target;

const FALLBACK: [(&str, &str); 29] = [
    ("gemini-3-flash", "Gemini 3 Flash"),
    ("gemini-3.5-flash", "Gemini 3.5 Flash"),
    ("gemini-3.5-flash-high", "Gemini 3.5 Flash (High)"),
    ("gemini-3.5-flash-low", "Gemini 3.5 Flash (Low)"),
    ("gemini-3.5-flash-extra-low", "Gemini 3.5 Flash (Extra Low)"),
    ("gemini-3.6-flash", "Gemini 3.6 Flash"),
    ("gemini-3.7-flash", "Gemini 3.7 Flash"),
    ("gemini-3.8-flash", "Gemini 3.8 Flash"),
    ("gemini-3.7-flash-low", "Gemini 3.7 Flash (Low)"),
    ("gemini-3.7-flash-medium", "Gemini 3.7 Flash (Medium)"),
    ("gemini-3.7-flash-high", "Gemini 3.7 Flash (High)"),
    ("gemini-3.1-pro-low", "Gemini 3.1 Pro (Low)"),
    ("gemini-3.1-pro-high", "Gemini 3.1 Pro (High)"),
    ("gemini-pro-agent", "Gemini Pro Agent"),
    ("gemini-3.1-pro-preview", "Gemini 3.1 Pro Preview"),
    ("gemini-3-pro-low", "Gemini 3 Pro (Low)"),
    ("gemini-3-pro-high", "Gemini 3 Pro (High)"),
    ("gemini-3-pro-preview", "Gemini 3 Pro Preview"),
    ("gemini-2.5-flash", "Gemini 2.5 Flash"),
    ("gemini-2.5-flash-lite", "Gemini 2.5 Flash Lite"),
    ("gemini-2.5-flash-thinking", "Gemini 2.5 Flash Thinking"),
    ("gemini-2.0-flash", "Gemini 2.0 Flash"),
    ("gemini-2.0-flash-exp", "Gemini 2.0 Flash Exp"),
    ("gemini-3-pro-image", "Gemini 3 Pro Image"),
    ("gemini-3.1-flash-image", "Gemini 3.1 Flash Image"),
    ("claude-sonnet-4-6", "Claude Sonnet 4.6"),
    ("claude-sonnet-4-6-thinking", "Claude Sonnet 4.6 Thinking"),
    ("claude-opus-4-6-thinking", "Claude Opus 4.6 Thinking"),
    ("gpt-oss-120b-medium", "GPT-OSS 120B (Medium)"),
];

pub fn is_disabled(id: &str, info: &Value) -> bool {
    let lower = id.trim().to_ascii_lowercase();
    info["disabled"].as_bool() == Some(true)
        || lower.contains("-disabled")
        || lower.ends_with("_disabled")
}

pub fn offered(id: &str) -> bool {
    let id = id.trim().to_ascii_lowercase();
    id.starts_with("gemini-") || id.starts_with("claude-") || id.starts_with("gpt-oss-")
}

pub fn claude(id: &str) -> bool {
    id.trim().to_ascii_lowercase().starts_with("claude-")
}

pub fn image(id: &str) -> bool {
    let id = id.trim().to_ascii_lowercase();
    id.starts_with("gemini-") && id.contains("image")
}

fn thinking(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    claude(&id)
        || id.starts_with("gpt-oss-")
        || id.starts_with("gemini-3")
        || id.starts_with("gemini-4")
        || argon::is_model(&id)
        || id.contains("pro-agent")
        || id.contains("thinking")
}

fn parse_version_token(token: &str) -> Option<(u32, u32)> {
    let cleaned = token.trim().trim_start_matches('v');
    let mut parts = cleaned.split(['.', '-', '_']);
    let major_tok = parts.next()?;
    if major_tok.len() == 8 && (major_tok.starts_with("202") || major_tok.starts_with("201")) {
        return None;
    }
    let major = major_tok.parse::<u32>().ok().filter(|v| *v < 1000)?;
    let minor = parts
        .next()
        .filter(|tok| !(tok.len() == 8 && (tok.starts_with("202") || tok.starts_with("201"))))
        .and_then(|tok| tok.parse::<u32>().ok())
        .filter(|v| *v < 1000)
        .unwrap_or(0);
    Some((major, minor))
}

pub(crate) fn claude_version(id: &str) -> Option<(u32, u32)> {
    let rest = id.find("claude").map(|pos| &id[pos..])?;
    let tokens: Vec<&str> = rest.split(['-', '_']).collect();
    let mut best: Option<(u32, u32)> = None;
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i];
        if !(token.len() == 8 && (token.starts_with("202") || token.starts_with("201"))) {
            if let Some(major) = token.parse::<u32>().ok().filter(|v| *v < 1000) {
                let mut minor = 0;
                if let Some(next_tok) = tokens.get(i + 1) {
                    if !(next_tok.len() == 8
                        && (next_tok.starts_with("202") || next_tok.starts_with("201")))
                    {
                        if let Some(m) = next_tok.parse::<u32>().ok().filter(|v| *v < 1000) {
                            minor = m;
                            i += 1;
                        }
                    }
                }
                let ver = (major, minor);
                best = Some(best.map_or(ver, |cur| cur.max(ver)));
            } else if let Some(ver) = parse_version_token(token) {
                best = Some(best.map_or(ver, |cur| cur.max(ver)));
            }
        }
        i += 1;
    }
    best
}

/// 判断模型是否应进入逐模型配额明细（参照 Antigravity-Manager 基准线规则并放行 Gemini 4+ 全系）。
pub fn is_quota_baseline_model(
    id: &str,
    info: &Value,
    in_official_sort: bool,
    allow_3x_flash_fallback: bool,
) -> bool {
    let lower = id.trim().to_ascii_lowercase();
    if is_disabled(id, info)
        || info["isInternal"].as_bool() == Some(true)
        || lower.starts_with("chat_")
        || lower.starts_with("tab_")
        || lower.contains("internal")
        || lower.contains("-exp")
        || lower.ends_with("-agent")
        || lower.ends_with("_vertex")
        || !offered(id)
    {
        return false;
    }
    if lower.starts_with("gemini-4")
        || lower.contains("argon")
        || matches!(
            lower.as_str(),
            "gemini-3.1-flash-lite"
                | "gemini-3.5-flash-lite"
                | "gemini-3.1-flash-image"
                | "gemini-3-pro-image"
        )
    {
        return true;
    }
    if lower.contains("claude") {
        if (lower.starts_with("claude-sonnet-") || lower.contains("-5-5"))
            && lower.contains("thinking")
        {
            return false;
        }
        let Some(ver) = claude_version(&lower) else {
            return false;
        };
        if lower.contains("haiku") || lower.contains("fable") {
            return in_official_sort && ver.0 >= 5;
        }
        return ver >= (4, 6);
    }
    if lower.starts_with("gpt-oss-") {
        return lower == "gpt-oss-120b-medium" || lower == "gpt-oss-120b";
    }
    if lower.contains("image") {
        return false;
    }
    if let Some(rest) = lower.strip_prefix("gemini-") {
        let ver = parse_version_token(rest);
        if rest.contains("pro") {
            return !lower.contains("preview") && ver.is_some_and(|v| v >= (3, 1));
        }
        if rest.contains("flash") {
            if ver.is_some_and(|v| v >= (3, 8)) {
                return true;
            }
            return allow_3x_flash_fallback
                && in_official_sort
                && (lower == "gemini-3-flash" || ver.is_some_and(|v| v >= (3, 7)));
        }
        return ver.is_some_and(|v| v.0 >= 4);
    }
    false
}

fn default_budget(id: &str) -> Option<i64> {
    if tiers::uses_thinking_level(id, None) {
        return None;
    }
    if claude(id) {
        Some(1024)
    } else if id.to_ascii_lowercase().starts_with("gpt-oss-") {
        Some(8192)
    } else {
        None
    }
}

fn build_raw(id: &str, info: &Value) -> Option<catalog_variants::RawModel> {
    let id = id.trim();
    if !offered(id) || is_disabled(id, info) || id.len() > 512 || id.chars().any(char::is_control) {
        return None;
    }
    let is_image = image(id);
    let display = util::text(info, &["displayName", "display_name", "name"]);
    let name = if display.is_empty() {
        id.to_string()
    } else {
        display
    };
    let supports_thinking = info
        .get("supportsThinking")
        .or_else(|| info.get("supports_thinking"))
        .or_else(|| info.get("supportsReasoning"))
        .and_then(Value::as_bool)
        .unwrap_or_else(|| !is_image && thinking(id));
    let mut entry = json!({
        "id": id,
        "name": name,
        "supportsToolCall": !is_image && info["supportsToolCall"].as_bool().unwrap_or(true),
        "supportsImages": info["supportsImages"].as_bool().unwrap_or(true),
        "supportsVideo": false,
        "supportsReasoning": supports_thinking,
        "supportsThinking": supports_thinking,
        "isDefault": !is_image && info["recommended"].as_bool().unwrap_or(id == "gemini-3-flash"),
        "visibility": info.get("visibility").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| {
            if info["isInternal"] == true || id.to_ascii_lowercase().starts_with("gemini-2.") {
                "hidden".to_string()
            } else {
                "shown".to_string()
            }
        }),
    });
    if is_image {
        entry["kind"] = json!("image");
        entry["output_modalities"] = json!(["image"]);
        entry["supports_image_generation"] = json!(true);
        entry["supports_image_editing"] = json!(true);
    }
    for (sources, to) in [
        (&["maxTokens", "maxInputTokens"][..], "maxInputTokens"),
        (&["maxOutputTokens"][..], "maxOutputTokens"),
    ] {
        if let Some(n) = sources
            .iter()
            .find_map(|key| info.get(*key).and_then(Value::as_i64))
            .filter(|n| *n > 0)
        {
            entry[to] = json!(n);
        }
    }
    let thinking_mode = if supports_thinking && tiers::uses_thinking_level(id, Some(info)) {
        "level"
    } else {
        "budget"
    };
    let upstream_thinking_level = tiers::upstream_thinking_level_wire(info);
    let raw_thinking_budget = info
        .get("thinkingBudget")
        .or_else(|| info.get("thinking_budget"))
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .or_else(|| (thinking_mode == "budget").then(|| default_budget(id)).flatten());
    if supports_thinking && thinking_mode == "budget" {
        if let Some(n) = raw_thinking_budget {
            entry["thinkingBudget"] = json!(n);
        }
    }
    if let Some(quota_src) = info
        .get("quotaInfo")
        .or_else(|| info.get("quota"))
        .filter(|value| value.is_object())
    {
        let mut quota = quota_src.clone();
        if let Some(fraction) = util::quota_fraction(&quota) {
            quota["remainingFraction"] = json!(fraction);
        }
        entry["quota"] = quota;
    }
    if info["supportedMimeTypes"].is_array() || info["supportedMimeTypes"].is_object() {
        entry["supportedMimeTypes"] = info["supportedMimeTypes"].clone();
    }
    let max_output_tokens = entry["maxOutputTokens"].as_i64().filter(|n| *n > 0);
    Some(catalog_variants::RawModel {
        id: id.to_string(),
        name,
        entry,
        thinking_mode,
        upstream_thinking_level,
        raw_thinking_budget,
        max_output_tokens,
    })
}

/// 判断一份缓存目录是否包含尚未合并的旧版带等级后缀条目。
pub fn needs_collapse(entries: &[Value]) -> bool {
    entries.iter().any(|entry| {
        let Some(id) = entry["id"].as_str().map(str::trim) else {
            return false;
        };
        entry.get("antigravityVariants").is_none()
            && tiers::strip_effort_id(id, false).1.is_some()
    })
}

/// 兼容旧缓存：对未合并的旧格式条目列表现场合并，已合并列表保持幂等不变。
pub fn collapse_cached_entries(entries: Vec<Value>) -> Vec<Value> {
    if !needs_collapse(&entries) {
        return entries;
    }
    let raw: Vec<_> = entries
        .into_iter()
        .filter_map(|entry| {
            let id = entry["id"].as_str()?.trim().to_string();
            build_raw(&id, &entry)
        })
        .collect();
    catalog_variants::collapse_raw(raw)
}

/// 将单组已知变体（如 Argon 专属额度补齐）按统一口径合并为单条基础模型。
pub fn collapse_pairs(pairs: &[(&str, Value)]) -> Vec<Value> {
    let raw: Vec<_> = pairs
        .iter()
        .filter_map(|(id, info)| build_raw(id, info))
        .collect();
    catalog_variants::collapse_raw(raw)
}

pub fn has_collection(body: &Value) -> bool {
    match &body["models"] {
        Value::Object(models) => {
            models.len() <= 4096
                && models
                    .iter()
                    .all(|(id, model)| !id.trim().is_empty() && model.is_object())
        }
        Value::Array(models) => {
            models.len() <= 4096
                && models.iter().all(|model| {
                    model.is_object()
                        && model["id"].as_str().is_some_and(|id| !id.trim().is_empty())
                })
        }
        _ => false,
    }
}

pub fn parse(body: &Value) -> Vec<Value> {
    let raw: Vec<_> = match &body["models"] {
        Value::Object(items) => items
            .iter()
            .filter_map(|(id, info)| build_raw(id, info))
            .collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(|info| build_raw(info["id"].as_str()?, info))
            .collect(),
        _ => Vec::new(),
    };
    catalog_variants::collapse_raw(raw)
}

pub fn fallback() -> Vec<Value> {
    let raw: Vec<_> = FALLBACK
        .iter()
        .filter_map(|(id, name)| build_raw(id, &json!({ "displayName": name })))
        .collect();
    catalog_variants::collapse_raw(raw)
}

pub fn entry(list: &[Value], id: &str) -> Option<Value> {
    let trimmed = id.trim();
    if let Some(found) = list
        .iter()
        .find(|row| row["id"].as_str().is_some_and(|c| c.eq_ignore_ascii_case(trimmed)))
    {
        return Some(found.clone());
    }
    if let Some(found) = list.iter().find(|row| {
        row["antigravityVariants"]
            .as_object()
            .is_some_and(|variants| {
                variants.values().any(|variant| {
                    variant["id"]
                        .as_str()
                        .is_some_and(|c| c.eq_ignore_ascii_case(trimmed))
                })
            })
    }) {
        return Some(found.clone());
    }
    let wire = aliases::upstream(trimmed);
    list.iter()
        .find(|row| row["id"].as_str().is_some_and(|c| c.eq_ignore_ascii_case(&wire)))
        .cloned()
}
