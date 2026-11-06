use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::{client, endpoints};

const CACHE_KEY: &str = "quworkModelCatalog";
const TTL_MS: i64 = 10 * 60_000;

#[derive(Clone)]
pub struct Catalog {
    pub config: Value,
    pub fetched_at: i64,
}

fn memory() -> &'static Mutex<Option<Catalog>> {
    static CACHE: OnceLock<Mutex<Option<Catalog>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

fn cached() -> Option<Catalog> {
    memory()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn save(catalog: Catalog) {
    *memory()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(catalog);
}

pub fn fallback() -> Catalog {
    Catalog {
        config: serde_json::from_str(include_str!("models.json")).unwrap_or(Value::Null),
        fetched_at: 0,
    }
}

pub fn validate(config: &Value) -> Result<(), String> {
    if config.get("key").and_then(Value::as_str) != Some("qurvis-desktop-model-config")
        || config.get("schemaVersion").and_then(Value::as_u64) != Some(1)
        || config.get("enabled").and_then(Value::as_bool).is_none()
    {
        return Err("QuWork 模型配置格式不兼容".into());
    }
    let providers = config
        .get("providers")
        .and_then(Value::as_array)
        .filter(|items| items.len() <= 8)
        .ok_or("QuWork 模型提供商配置异常")?;
    let mut ids = HashSet::new();
    for provider in providers {
        if provider.get("id").and_then(Value::as_str) != Some("qurwork") {
            continue;
        }
        if provider.get("api").and_then(Value::as_str) != Some("openai-completions") {
            return Err("QuWork 模型接口协议已变化，请更新插件".into());
        }
        if provider
            .get("baseURL")
            .and_then(Value::as_str)
            .map(|url| url.trim_end_matches('/'))
            != Some(endpoints::MODEL_ORIGIN)
        {
            return Err("QuWork 模型服务地址已变化，未向未确认的地址发送凭证".into());
        }
        let rows = provider
            .get("models")
            .and_then(Value::as_array)
            .filter(|rows| rows.len() <= 128)
            .ok_or("QuWork 模型清单格式异常")?;
        for row in rows {
            let id = row
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
                .ok_or("QuWork 模型名称格式异常")?;
            if !ids.insert(id) {
                return Err("QuWork 模型清单存在重复名称".into());
            }
            if row.get("enabled").and_then(Value::as_bool).is_none() {
                return Err("QuWork 模型启用状态不明确".into());
            }
        }
    }
    if !providers
        .iter()
        .any(|provider| provider.get("id").and_then(Value::as_str) == Some("qurwork"))
    {
        return Err("QuWork 模型配置未包含模型提供商".into());
    }
    Ok(())
}

pub async fn current(ctx: &PluginCtx) -> Catalog {
    if let Some(catalog) = cached() {
        return catalog;
    }
    if let Ok(saved) = ctx.settings_get(endpoints::PROVIDER, CACHE_KEY).await {
        if let Some(config) = saved
            .get("config")
            .filter(|config| validate(config).is_ok())
        {
            let catalog = Catalog {
                config: config.clone(),
                fetched_at: saved.get("fetchedAt").and_then(Value::as_i64).unwrap_or(0),
            };
            save(catalog.clone());
            return catalog;
        }
    }
    fallback()
}

impl Catalog {
    pub fn rows(&self) -> Vec<&Value> {
        if self.config.get("enabled").and_then(Value::as_bool) != Some(true) {
            return Vec::new();
        }
        let mut rows: Vec<&Value> = self
            .config
            .get("providers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|provider| provider.get("id").and_then(Value::as_str) == Some("qurwork"))
            .flat_map(|provider| {
                provider
                    .get("models")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .filter(|model| model.get("enabled").and_then(Value::as_bool) == Some(true))
            .collect();
        rows.sort_by_key(|row| row.get("sort").and_then(Value::as_i64).unwrap_or(i64::MAX));
        rows
    }

    pub fn model(&self, id: &str) -> Option<&Value> {
        self.rows()
            .into_iter()
            .find(|row| row.get("id").and_then(Value::as_str) == Some(id))
    }

    pub fn list(&self) -> proto::ModelList {
        let default = self
            .config
            .pointer("/defaultModel/modelId")
            .and_then(Value::as_str)
            .unwrap_or("");
        let models = self.rows().into_iter().map(|row| {
            let id = row.get("id").and_then(Value::as_str).unwrap_or("");
            let efforts = row.get("reasoningEfforts").and_then(Value::as_object);
            let mut model = json!({
                "id": id,
                "name": row.get("name").and_then(Value::as_str).unwrap_or(id),
                "supportsToolCall": true,
                "supportsImages": row.get("supportsImage").and_then(Value::as_bool).unwrap_or(false),
                "isDefault": id == default,
            });
            for (target, source) in [("maxInputTokens", "contextWindow"), ("maxOutputTokens", "maxTokens")] {
                if let Some(value) = row.get(source).and_then(Value::as_u64).filter(|value| *value > 0) {
                    model[target] = json!(value);
                }
            }
            if let Some(efforts) = efforts {
                model["supportsReasoning"] = json!(true);
                model["reasoningLevels"] = json!(efforts.keys().collect::<Vec<_>>());
            }
            model
        }).collect();
        proto::ModelList {
            models,
            meta: proto::CatalogMeta {
                remote: self.fetched_at > 0,
                refreshed_at: self.fetched_at,
            },
        }
    }
}

pub fn default_models() -> Vec<Value> {
    fallback().list().models
}

pub async fn refresh(ctx: &PluginCtx, force: bool) -> Result<proto::RefreshOutcome, String> {
    let catalog = current(ctx).await;
    if !force
        && catalog.fetched_at > 0
        && crate::http::now_ms().saturating_sub(catalog.fetched_at) < TTL_MS
    {
        return Ok(if catalog.list().models.is_empty() {
            proto::RefreshOutcome::failed("QuWork 当前未开放可用模型")
        } else {
            proto::RefreshOutcome::unchanged()
        });
    }
    crate::flight::run("quwork-models", || async {
        let config = client::public_config(&client::client(ctx, None))
            .await
            .map_err(|error| error.message)?;
        validate(&config)?;
        let catalog = Catalog {
            config,
            fetched_at: crate::http::now_ms(),
        };
        ctx.settings_set(
            endpoints::PROVIDER,
            CACHE_KEY,
            &json!({ "config": catalog.config, "fetchedAt": catalog.fetched_at }),
        )
        .await
        .map_err(|_| "QuWork 模型目录保存失败，未替换原目录".to_string())?;
        let count = catalog.list().models.len();
        save(catalog);
        if count == 0 {
            return Ok(proto::RefreshOutcome::failed(
                "QuWork 当前未开放可用模型，未发送任何模型请求",
            ));
        }
        Ok(proto::RefreshOutcome::refreshed(count))
    })
    .await
}
