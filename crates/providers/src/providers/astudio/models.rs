//! AStudio 模型目录：按账号实时拉取（读取不计费），带进程内缓存与静态兜底。
//!
//! ── 上游形状 ────────────────────────────────────────────────
//! ```json
//! {"code":0,"data":{"models":[{"slug":"spark-x2.6","display_name":"Spark-X2.6",
//!   "context_window":262144,"max_output_tokens":32768,
//!   "input_modalities":["text","image"],
//!   "supported_reasoning_levels":[{"effort":"none"},{"effort":"high"}],
//!   "badge":{"text":{"en":"Free"}},"visibility":"public","supported_in_api":true}]}}
//! ```
//!
//! ── 缓存 ────────────────────────────────────────────────────
//! 进程内按账号 10 分钟；手动刷新绕过。不落盘（网关自己会缓存插件清单）。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::accounts::Credentials;
use super::client::{self, CatalogError};
use super::PROVIDER_ID;

const TTL_MS: i64 = 10 * 60 * 1000;

struct Catalog {
    models: Vec<Value>,
    fetched_at: i64,
}

fn cache() -> &'static Mutex<HashMap<String, Catalog>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Catalog>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_cache<T>(action: impl FnOnce(&mut HashMap<String, Catalog>) -> T) -> T {
    match cache().lock() {
        Ok(mut table) => action(&mut table),
        Err(poisoned) => action(&mut poisoned.into_inner()),
    }
}

fn now_ms() -> i64 {
    super::accounts::now_ms()
}

/// 静态兜底（参考实现 2026-11-01 的实测快照）。
pub fn fallback_models() -> Vec<Value> {
    let rows: [(&str, &str, i64, i64, bool); 7] = [
        ("astronclaw-auto", "Auto", 262_144, 32_768, false),
        ("spark-x2.6", "Spark-X2.6", 262_144, 32_768, false),
        ("spark-x2.6-4b", "Spark-X2.6-Lite", 1_048_576, 110_000, true),
        ("spark-x2.5", "Spark-X2.5", 262_144, 32_768, false),
        ("xopglm52", "GLM-5.2", 1_048_576, 32_768, false),
        (
            "xopdeepseekv4pro0813",
            "DeepSeek-V4-Pro",
            1_048_576,
            32_768,
            false,
        ),
        (
            "xopdsv4flash0731in",
            "DeepSeek-V4-Flash",
            1_048_576,
            32_768,
            false,
        ),
    ];
    rows.iter()
        .map(|(id, name, context, output, free)| {
            let mut record = json!({
                "id": id,
                "name": name,
                "maxInputTokens": context,
                "maxOutputTokens": output,
                "supportsToolCall": true,
                "supportsReasoning": true,
                "isDefault": *id == "astronclaw-auto",
            });
            if *free {
                record["free"] = json!(true);
            }
            record
        })
        .collect()
}

fn row_of(entry: &Value) -> Option<Value> {
    let id = entry
        .get("slug")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    if entry.get("visibility").and_then(Value::as_str) == Some("hidden") {
        return None;
    }
    if entry.get("supported_in_api").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let name = entry
        .get("display_name")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(id);
    let context = entry
        .get("context_window")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .unwrap_or(0);
    let output = entry
        .get("max_output_tokens")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .unwrap_or(32_768);
    let images = entry
        .get("input_modalities")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.as_str().is_some_and(|text| text == "image"))
        });
    let reasoning = entry
        .get("supported_reasoning_levels")
        .and_then(Value::as_array)
        .is_some_and(|levels| {
            levels.iter().any(|level| {
                level
                    .get("effort")
                    .and_then(Value::as_str)
                    .is_some_and(|effort| !effort.is_empty() && effort != "none")
            })
        });
    let free = entry
        .pointer("/badge/text/en")
        .and_then(Value::as_str)
        .is_some_and(|text| text == "Free");
    let mut record = json!({
        "id": id,
        "name": name,
        "maxInputTokens": context,
        "maxOutputTokens": output,
        "supportsToolCall": true,
        "supportsReasoning": reasoning,
        "isDefault": id == "astronclaw-auto",
    });
    if images {
        record["supportsImages"] = json!(true);
    }
    if free {
        record["free"] = json!(true);
    }
    Some(record)
}

fn parse(payload: &Value) -> Result<Vec<Value>, String> {
    let list = payload
        .pointer("/data/models")
        .and_then(Value::as_array)
        .ok_or_else(|| "AStudio 模型目录格式异常".to_string())?;
    let mut models: Vec<Value> = list.iter().filter_map(row_of).collect();
    if models.is_empty() {
        return Err("AStudio 模型目录为空".to_string());
    }
    models.sort_by_key(|model| {
        if model.get("isDefault").and_then(Value::as_bool) == Some(true) {
            0
        } else {
            1
        }
    });
    Ok(models)
}

/// 当前清单（缓存优先；没有缓存用兜底）。
pub fn list(account: &str) -> proto::ModelList {
    let cached = with_cache(|table| {
        table.get(account).and_then(|catalog| {
            (now_ms() - catalog.fetched_at < TTL_MS)
                .then(|| (catalog.models.clone(), catalog.fetched_at))
        })
    });
    match cached {
        Some((models, fetched_at)) => proto::ModelList {
            models,
            meta: proto::CatalogMeta {
                remote: true,
                refreshed_at: fetched_at,
            },
        },
        None => proto::ModelList {
            models: fallback_models(),
            meta: Default::default(),
        },
    }
}

/// 拉一次目录并写缓存。
pub async fn refresh(
    ctx: &PluginCtx,
    account: &str,
    force: bool,
) -> Result<(proto::RefreshOutcome, Vec<Value>), String> {
    let stored = ctx
        .store_get(PROVIDER_ID, account)
        .await
        .map_err(|error| format!("读取 AStudio 账号失败：{error}"))?;
    let credentials = Credentials::from_secret(&stored.secret);
    if credentials.key.is_empty() {
        return Err("AStudio 账号缺少 modelBearerToken，请重新添加".to_string());
    }
    if !force {
        let cached = with_cache(|table| {
            table
                .get(&stored.id)
                .filter(|catalog| now_ms() - catalog.fetched_at < TTL_MS)
                .map(|catalog| catalog.models.clone())
        });
        if let Some(models) = cached {
            return Ok((proto::RefreshOutcome::unchanged(), models));
        }
    }
    let payload = match client::fetch_catalog(&credentials.key).await {
        Ok(payload) => payload,
        Err(CatalogError::Expired) => {
            return Err("AStudio 拒绝了这份凭据（HTTP 401/403），请重新登录".into())
        }
        Err(CatalogError::Failed(message)) => {
            return Err(format!("拉取 AStudio 模型目录失败：{message}"))
        }
    };
    let models = parse(&payload)?;
    let count = models.len();
    with_cache(|table| {
        table.insert(
            stored.id.clone(),
            Catalog {
                models: models.clone(),
                fetched_at: now_ms(),
            },
        );
    });
    Ok((proto::RefreshOutcome::refreshed(count), models))
}
