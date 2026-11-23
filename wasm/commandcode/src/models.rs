//! Command Code 公共模型目录与5分钟缓存；公共清单不等同于某账号套餐授权。
use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};
use std::collections::BTreeSet;

use super::{account, config::Config, errors};

const TTL_MS: i64 = 5 * 60 * 1000;
const FALLBACK_MODELS: [(&str, &str); 26] = [
    ("claude-sonnet-4-6", "Claude Sonnet 4.6"),
    ("claude-opus-4-8", "Claude Opus 4.8"),
    ("claude-opus-4-7", "Claude Opus 4.7"),
    ("claude-haiku-4-5-20251001", "Claude Haiku 4.5"),
    ("gpt-5.5", "GPT-5.5"),
    ("gpt-5.4", "GPT-5.4"),
    ("gpt-5.4-mini", "GPT-5.4 Mini"),
    ("gpt-5.3-codex", "GPT-5.3 Codex"),
    ("deepseek/deepseek-v4-pro", "DeepSeek V4 Pro"),
    ("deepseek/deepseek-v4-flash", "DeepSeek V4 Flash"),
    ("moonshotai/Kimi-K2.6", "Kimi K2.6"),
    ("moonshotai/Kimi-K2.5", "Kimi K2.5"),
    ("zai-org/GLM-5.1", "GLM 5.1"),
    ("zai-org/GLM-5", "GLM 5"),
    ("MiniMaxAI/MiniMax-M3", "MiniMax M3"),
    ("MiniMaxAI/MiniMax-M2.7", "MiniMax M2.7"),
    ("MiniMaxAI/MiniMax-M2.5", "MiniMax M2.5"),
    ("Qwen/Qwen3.6-Max-Preview", "Qwen 3.6 Max Preview"),
    ("Qwen/Qwen3.6-Plus", "Qwen 3.6 Plus"),
    ("Qwen/Qwen3.7-Max", "Qwen 3.7 Max"),
    ("stepfun/Step-3.7-Flash", "Step 3.7 Flash"),
    ("stepfun/Step-3.5-Flash", "Step 3.5 Flash"),
    ("xiaomi/mimo-v2.5-pro", "MiMo V2.5 Pro"),
    ("xiaomi/mimo-v2.5", "MiMo V2.5"),
    ("google/gemini-3.5-flash", "Gemini 3.5 Flash"),
    ("google/gemini-3.1-flash-lite", "Gemini 3.1 Flash Lite"),
];

fn model(id: &str, name: &str) -> Value {
    let lower = id.to_ascii_lowercase();
    // 公开CLI表与Provider目录的明确图片能力；未知模型不凭名字通用猜成支持。
    let images = lower.starts_with("claude-")
        || lower.starts_with("anthropic/claude-")
        || lower.starts_with("gpt-5")
        || lower.starts_with("gpt-6")
        || lower.starts_with("google/gemini-")
        || lower.starts_with("moonshotai/kimi-k2")
        || lower == "moonshotai/kimi-k3"
        || lower == "xiaomi/mimo-v2.5"
        || lower.starts_with("xiaomi/mimo-v2.6")
        || lower == "qwen/qwen3.6-plus"
        || lower.starts_with("qwen/qwen3.7-plus")
        || lower.starts_with("qwen/qwen3.7-flash")
        || lower.starts_with("qwen/qwen3.8")
        || lower == "stepfun/step-3.7-flash"
        || lower == "minimaxai/minimax-m3"
        || lower.contains("vision");
    json!({ "id": id, "name": name, "supportsToolCall": true, "supportsReasoning": true, "supportsImages": images })
}

pub fn fallback_models() -> Vec<Value> {
    FALLBACK_MODELS
        .iter()
        .map(|(id, name)| model(id, name))
        .collect()
}

pub fn cached() -> Result<Value> {
    let cached = sdk::catalog::cached()?;
    if cached["models"].is_array() && cached["meta"].is_object() {
        // 远程成功的空目录是有效快照，不能偷偷回滚到静态26项。
        Ok(json!({ "models": cached["models"], "meta": cached["meta"] }))
    } else {
        Ok(json!({ "models": fallback_models(), "meta": { "remote": false, "refreshed_at": 0 } }))
    }
}

fn outcome(mut list: Value, refreshed: bool, message: Option<String>) -> Value {
    list["refreshed"] = json!(refreshed);
    list["count"] = json!(list["models"].as_array().map(Vec::len).unwrap_or(0));
    if let Some(message) = message {
        list["message"] = json!(message);
    }
    list
}

fn fetch(config: &Config) -> Result<Value> {
    // 官方CLI及公开Provider API都以匿名GET读取相同公共清单，Go Key的套餐拒绝不应挡目录。
    let response = http::open_timeout(
        "GET",
        &config.url("/provider/v1/models"),
        json!({ "accept": "application/json", "user-agent": "node" }),
        None,
        10_000,
    )?;
    let status = response.status;
    let bytes = response.bytes(4 * 1024 * 1024)?;
    let payload: Value = serde_json::from_slice(&bytes)
        .map_err(|_| Error::http(502, "Command Code 模型目录未返回有效 JSON"))?;
    if !(200..300).contains(&status) {
        return Err(errors::http_error(status, &payload));
    }
    if payload.get("error").is_some_and(|error| !error.is_null()) || payload["success"] == false {
        return Err(errors::http_error(502, &payload));
    }
    let rows = payload["data"]
        .as_array()
        .or_else(|| payload.as_array())
        .ok_or_else(|| Error::http(502, "Command Code 模型目录格式无法识别"))?;
    if rows.len() > 4096 {
        return Err(Error::http(502, "Command Code 模型目录超过条目限制"));
    }
    let mut seen = BTreeSet::new();
    let mut models = Vec::new();
    for row in rows {
        let id = row["id"]
            .as_str()
            .map(str::trim)
            .filter(|id| !id.is_empty() && id.len() <= 512 && !id.chars().any(char::is_control))
            .ok_or_else(|| Error::http(502, "Command Code 模型条目缺少合法标识"))?;
        if !seen.insert(id.to_ascii_lowercase()) {
            continue;
        }
        let name = row["name"]
            .as_str()
            .map(str::trim)
            .filter(|name| !name.is_empty() && name.len() <= 512)
            .or_else(|| {
                row["display_name"]
                    .as_str()
                    .filter(|name| !name.is_empty() && name.len() <= 512)
            })
            .unwrap_or(id);
        let mut entry = model(id, name);
        if let Some(modalities) = row["modalities"]["input"].as_array() {
            entry["supportsImages"] = json!(modalities.iter().any(|value| value == "image"));
        }
        for (remote, local) in [
            ("context_length", "maxInputTokens"),
            ("max_output_tokens", "maxOutputTokens"),
        ] {
            if let Some(value) = row[remote].as_u64().filter(|value| *value > 0) {
                entry[local] = json!(value);
            }
        }
        models.push(entry);
    }
    let list = json!({ "models": models, "meta": { "remote": true, "refreshed_at": sdk::now() } });
    if list.to_string().len() > 256 * 1024 {
        return Err(Error::http(502, "Command Code 模型目录超过快照大小限制"));
    }
    Ok(list)
}

pub fn refresh(params: &Value) -> Result<Value> {
    let previous = cached()?;
    if !params["force"].as_bool().unwrap_or(false) {
        let age = sdk::now().saturating_sub(previous["meta"]["refreshed_at"].as_i64().unwrap_or(0));
        if previous["meta"]["remote"] == true && (0..TTL_MS).contains(&age) {
            return Ok(outcome(previous, false, None));
        }
    }
    let record = if params["account"].as_str().is_some_and(|id| !id.is_empty()) {
        Some(sdk::account()?)
    } else {
        None
    };
    let config = Config::load()?;
    let result = fetch(&config);
    if let Some(record) = &record {
        account::unchanged(record)?;
    }
    match result {
        Ok(list) => Ok(outcome(list, true, None)),
        Err(error) if errors::host_failure(&error) => Err(error),
        Err(error) => Ok(outcome(
            previous,
            false,
            Some(format!(
                "Command Code 目录未能刷新，已保留当前清单：{}",
                errors::redact_text(&error.message, "")
            )),
        )),
    }
}
