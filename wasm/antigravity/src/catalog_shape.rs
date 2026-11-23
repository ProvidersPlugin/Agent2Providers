//! 原生目录的模型范围、29条兜底和展示能力。只广告三家族，不从订阅名扩大权限。

use super::{aliases, argon, util};
use agent2api_wasm_sdk::{json, Value};

const FALLBACK: [(&str, &str); 29] = [
    ("gemini-3-flash", "Gemini 3 Flash"),
    ("gemini-3.5-flash", "Gemini 3.5 Flash"),
    ("gemini-3.5-flash-high", "Gemini 3.5 Flash High"),
    ("gemini-3.5-flash-low", "Gemini 3.5 Flash Low"),
    ("gemini-3.5-flash-extra-low", "Gemini 3.5 Flash Extra Low"),
    ("gemini-3.6-flash", "Gemini 3.6 Flash"),
    ("gemini-3.7-flash", "Gemini 3.7 Flash"),
    ("gemini-3.8-flash", "Gemini 3.8 Flash"),
    ("gemini-3.7-flash-low", "Gemini 3.7 Flash Low"),
    ("gemini-3.7-flash-medium", "Gemini 3.7 Flash Medium"),
    ("gemini-3.7-flash-high", "Gemini 3.7 Flash High"),
    ("gemini-3.1-pro-low", "Gemini 3.1 Pro Low"),
    ("gemini-3.1-pro-high", "Gemini 3.1 Pro High"),
    ("gemini-pro-agent", "Gemini Pro Agent"),
    ("gemini-3.1-pro-preview", "Gemini 3.1 Pro Preview"),
    ("gemini-3-pro-low", "Gemini 3 Pro Low"),
    ("gemini-3-pro-high", "Gemini 3 Pro High"),
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
    ("gpt-oss-120b-medium", "GPT-OSS 120B Medium"),
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
fn claude_version(id: &str) -> Option<(u32, u32)> {
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
    if claude(id) {
        Some(1024)
    } else if id.to_ascii_lowercase().starts_with("gpt-oss-") {
        Some(8192)
    } else {
        None
    }
}

pub fn map_entry(id: &str, info: &Value) -> Option<Value> {
    let id = id.trim();
    if !offered(id) || is_disabled(id, info) || id.len() > 512 || id.chars().any(char::is_control) {
        return None;
    }
    let image = image(id);
    let name = util::text(info, &["displayName"]);
    let thinking = info["supportsThinking"]
        .as_bool()
        .unwrap_or_else(|| thinking(id));
    let mut entry = json!({ "id": id, "name": if name.is_empty() { id.to_string() } else { name },
        "supportsToolCall": !image && info["supportsToolCall"].as_bool().unwrap_or(true), "supportsImages": info["supportsImages"].as_bool().unwrap_or(true),
        "supportsVideo": false, "supportsReasoning": thinking, "supportsThinking": thinking,
        "isDefault": !image && info["recommended"].as_bool().unwrap_or(id == "gemini-3-flash"),
        "visibility": if info["isInternal"] == true || id.to_ascii_lowercase().starts_with("gemini-2.") { "hidden" } else { "shown" } });
    if image {
        entry["kind"] = json!("image");
        entry["output_modalities"] = json!(["image"]);
        entry["supports_image_generation"] = json!(true);
        entry["supports_image_editing"] = json!(true);
    }
    for (from, to) in [
        ("maxTokens", "maxInputTokens"),
        ("maxOutputTokens", "maxOutputTokens"),
    ] {
        if let Some(n) = info[from].as_i64().filter(|n| *n > 0) {
            entry[to] = json!(n);
        }
    }
    if thinking {
        if let Some(n) = info["thinkingBudget"]
            .as_i64()
            .filter(|n| *n > 0)
            .or_else(|| default_budget(id))
        {
            entry["thinkingBudget"] = json!(n);
        }
    }
    if info["quotaInfo"].is_object() {
        let mut quota = info["quotaInfo"].clone();
        if let Some(fraction) = util::quota_fraction(&quota) {
            quota["remainingFraction"] = json!(fraction);
        }
        entry["quota"] = quota;
    }
    if info["supportedMimeTypes"].is_array() || info["supportedMimeTypes"].is_object() {
        entry["supportedMimeTypes"] = info["supportedMimeTypes"].clone();
    }
    Some(entry)
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
    match &body["models"] {
        Value::Object(items) => items
            .iter()
            .filter_map(|(id, info)| map_entry(id, info))
            .collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(|info| map_entry(info["id"].as_str()?, info))
            .collect(),
        _ => Vec::new(),
    }
}
pub fn fallback() -> Vec<Value> {
    FALLBACK
        .iter()
        .filter_map(|(id, name)| map_entry(id, &json!({ "displayName": name })))
        .collect()
}
pub fn entry(list: &[Value], id: &str) -> Option<Value> {
    let wire = aliases::upstream(id);
    list.iter()
        .find(|row| {
            row["id"]
                .as_str()
                .is_some_and(|candidate| candidate == id.trim() || candidate == wire)
        })
        .cloned()
}
