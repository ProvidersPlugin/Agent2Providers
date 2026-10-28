//! 模型目录：`GET /api/aioffice/v1/sessions/plans` 的 `data.models[]`。
//!
//! 上游给的是**模型能力表**（名称、倍率、上下文、是否多模态、思考档位），
//! 正是网关目录需要的；拉不到时用 2026-10-28 实测的 10 条内置表兜底。
//!
//! 对外 id 统一加 `lingxi/` 前缀：`GLM-5.3` / `DeepSeek-V4-Pro` 这些名字在
//! codearts / loomy / officeace 等同名，不加前缀会被聚合清单先到先得吃掉。

use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use agent2api_provider_protocol as proto;
use serde_json::{json, Value};

use agent2api_provider_sdk::PluginCtx;

use super::constants;
use super::http;
use super::account;

/// 目录缓存：一次刷新给后续 `models` 调用复用（进程级）。
struct Cache {
    models: Vec<Value>,
    fetched_at: i64,
}

static CACHE: OnceLock<Mutex<Option<Cache>>> = OnceLock::new();

fn cache() -> &'static Mutex<Option<Cache>> {
    CACHE.get_or_init(|| Mutex::new(None))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or(0)
}

/// 内置兜底清单。
pub fn fallback() -> Vec<Value> {
    constants::FALLBACK_MODELS
        .iter()
        .map(|model| {
            json!({
                "id": format!("{}{}", constants::MODEL_PREFIX, model.key),
                "name": model.name,
                "credits": model.multiplier,
                "maxInputTokens": model.max_input,
                "maxOutputTokens": model.max_output,
                "supportsImages": model.vision,
                "supportsVideo": false,
                "supportsToolCall": true,
                "supportsReasoning": model.reasoning,
            })
        })
        .collect()
}

fn snapshot() -> Option<Cache> {
    cache()
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|cache| Cache { models: cache.models.clone(), fetched_at: cache.fetched_at }))
}

fn store(models: Vec<Value>) {
    if let Ok(mut guard) = cache().lock() {
        *guard = Some(Cache { models, fetched_at: now_ms() });
    }
}

/// 把上游 `data.models[]` 映射成网关认的记录。
fn map_models(payload: &Value) -> Vec<Value> {
    let data = http::data_of(payload);
    let Some(models) = data.get("models").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for model in models {
        let key = model
            .get("key")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if key.is_empty() {
            continue;
        }
        let name = model
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(key);
        let reasoning = model
            .get("reasoning_efforts")
            .and_then(Value::as_array)
            .map(|items| !items.is_empty())
            .unwrap_or(false);
        out.push(json!({
            "id": format!("{}{}", constants::MODEL_PREFIX, key),
            "name": name,
            "credits": model.get("multiplier").and_then(Value::as_str).unwrap_or(""),
            "maxInputTokens": model
                .get("max_input_tokens")
                .or_else(|| model.get("max_context_tokens"))
                .and_then(Value::as_i64)
                .unwrap_or(0),
            "maxOutputTokens": model
                .get("max_new_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            "supportsImages": model.get("support_multimodal").and_then(Value::as_bool).unwrap_or(false),
            "supportsVideo": false,
            "supportsToolCall": true,
            "supportsReasoning": reasoning,
            // 下面两个是本家内部字段（网关不读，留作排障）
            "provider_key": key,
            "intro": model.get("intro").and_then(Value::as_str).unwrap_or(""),
        }));
    }
    out
}

/// 拉一次远程目录（失败返回 Err，调用方决定兜底）。
pub async fn fetch(ctx: &PluginCtx, account: &str, proxy: Option<&proto::WireProxy>) -> Result<Vec<Value>, String> {
    let sid = account::sid_for(ctx, account).await?;
    let client = http::build_client(proxy, &ctx.env.proxy);
    let payload = http::get_json(&client, &sid, constants::PLANS_PATH).await?;
    let models = map_models(&payload);
    if models.is_empty() {
        return Err("灵犀目录里没有模型（结构漂移？）".to_string());
    }
    store(models.clone());
    Ok(models)
}

/// 当前清单：远程优先，失败或没刷过就内置兜底。
pub async fn list(ctx: &PluginCtx, account: &str, proxy: Option<&proto::WireProxy>) -> proto::ModelList {
    if let Some(cache) = snapshot() {
        if now_ms() - cache.fetched_at < 6 * 3600 * 1000 {
            return proto::ModelList {
                models: cache.models,
                meta: proto::CatalogMeta { remote: true, refreshed_at: cache.fetched_at },
            };
        }
    }
    match fetch(ctx, account, proxy).await {
        Ok(models) => {
            let refreshed_at = now_ms();
            proto::ModelList {
                models,
                meta: proto::CatalogMeta { remote: true, refreshed_at },
            }
        }
        Err(_) => {
            if let Some(cache) = snapshot() {
                return proto::ModelList {
                    models: cache.models,
                    meta: proto::CatalogMeta { remote: true, refreshed_at: cache.fetched_at },
                };
            }
            proto::ModelList {
                models: fallback(),
                meta: proto::CatalogMeta { remote: false, refreshed_at: 0 },
            }
        }
    }
}

/// 手动 / 定时刷新。
pub async fn refresh(
    ctx: &PluginCtx,
    account: &str,
    proxy: Option<&proto::WireProxy>,
    force: bool,
) -> proto::RefreshOutcome {
    if !force {
        if let Some(cache) = snapshot() {
            if now_ms() - cache.fetched_at < 6 * 3600 * 1000 {
                return proto::RefreshOutcome::unchanged();
            }
        }
    }
    match fetch(ctx, account, proxy).await {
        Ok(models) => proto::RefreshOutcome::refreshed(models.len()),
        Err(message) => proto::RefreshOutcome::failed(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_upstream_model_fields() {
        let payload = json!({
            "data": { "models": [{
                "key": "GLM-5.3",
                "name": "GLM-5.3",
                "multiplier": "~0.75x",
                "support_multimodal": false,
                "max_context_tokens": 1048576,
                "max_input_tokens": 1048576,
                "max_new_tokens": 131072,
                "reasoning_efforts": ["low", "high"]
            }] }
        });
        let models = map_models(&payload);
        assert_eq!(models.len(), 1);
        let model = &models[0];
        assert_eq!(model["id"], "lingxi/GLM-5.3");
        assert_eq!(model["credits"], "~0.75x");
        assert_eq!(model["maxInputTokens"], 1048576);
        assert_eq!(model["maxOutputTokens"], 131072);
        assert_eq!(model["supportsImages"], false);
        assert_eq!(model["supportsReasoning"], true);
    }

    #[test]
    fn fallback_has_ten_prefixed_models() {
        let models = fallback();
        assert_eq!(models.len(), 10);
        assert!(models.iter().all(|model| model["id"].as_str().unwrap_or("").starts_with("lingxi/")));
    }
}
