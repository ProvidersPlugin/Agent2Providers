//! WorkBuddy 两地各自的目录快照；持久化仍由网关负责。

use std::sync::{Mutex, OnceLock};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::Value;

use super::{builtin, region::Region};

const SETTING_MODEL_CATALOG: &str = "modelCatalog";

fn slot(region: Region) -> &'static Mutex<Option<proto::ModelList>> {
    static CN: OnceLock<Mutex<Option<proto::ModelList>>> = OnceLock::new();
    static INTL: OnceLock<Mutex<Option<proto::ModelList>>> = OnceLock::new();
    match region {
        Region::Cn => CN.get_or_init(|| Mutex::new(None)),
        Region::Intl => INTL.get_or_init(|| Mutex::new(None)),
    }
}

pub fn list(region: Region) -> proto::ModelList {
    slot(region).lock().ok().and_then(|guard| guard.clone())
        .unwrap_or_else(|| proto::ModelList {
            models: builtin::builtin_filtered(),
            meta: proto::CatalogMeta::default(),
        })
}

pub async fn current(ctx: &PluginCtx, region: Region) -> proto::ModelList {
    if slot(region).lock().map(|guard| guard.is_some()).unwrap_or(false) {
        return list(region);
    }
    let cached = ctx.settings_get(region.provider_id(), SETTING_MODEL_CATALOG).await
        .ok().and_then(|value| serde_json::from_value::<proto::ModelList>(value).ok())
        .filter(|list| list.meta.remote && !list.models.is_empty());
    if let Some(cached) = cached {
        if let Ok(mut guard) = slot(region).lock() {
            if guard.is_none() {
                *guard = Some(cached);
            }
        }
    }
    list(region)
}

pub fn store(region: Region, models: Vec<Value>) {
    if models.is_empty() {
        return;
    }
    if let Ok(mut guard) = slot(region).lock() {
        *guard = Some(proto::ModelList {
            models,
            meta: proto::CatalogMeta {
                remote: true,
                refreshed_at: crate::logging::now_ms(),
            },
        });
    }
}
