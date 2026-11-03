//! 两地展示目录独立；企业新增项不会成为个人账号的授权依据。

use std::sync::{Mutex, OnceLock};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

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

pub fn model(region: Region, id: &str) -> Option<Value> {
    list(region).models.into_iter().find(|model| {
        model
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|value| value.eq_ignore_ascii_case(id))
    })
}

pub fn list(region: Region) -> proto::ModelList {
    let mut list = slot(region)
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
        .unwrap_or_else(|| proto::ModelList {
            models: builtin::builtin_filtered(),
            meta: proto::CatalogMeta::default(),
        });
    for model in &mut list.models {
        super::reasoning::annotate(model);
    }
    list
}

pub async fn current(ctx: &PluginCtx, region: Region) -> proto::ModelList {
    if slot(region)
        .lock()
        .map(|guard| guard.is_some())
        .unwrap_or(false)
    {
        return list(region);
    }
    let cached = ctx
        .settings_get(region.provider_id(), SETTING_MODEL_CATALOG)
        .await
        .ok()
        .and_then(|value| serde_json::from_value::<proto::ModelList>(value).ok())
        .filter(|list| list.meta.remote && !list.models.is_empty());
    let mut restored = cached.unwrap_or_else(|| proto::ModelList {
        models: builtin::builtin_filtered(),
        meta: proto::CatalogMeta::default(),
    });
    if let Ok(accounts) = ctx.store_list(region.provider_id()).await {
        for entry in accounts
            .iter()
            .filter(|entry| entry.public.get("enabled").and_then(Value::as_bool) != Some(false))
        {
            let record = crate::store::record_of(entry);
            let session = super::session_of_record(&record, region);
            if !super::catalog_access::enterprise(&session) {
                continue;
            }
            let Some(access) = super::catalog_access::load(ctx, &session).await else {
                continue;
            };
            if !access.models.is_empty() {
                merge_models(&mut restored.models, &access.models);
                restored.meta.remote = true;
                restored.meta.refreshed_at = restored.meta.refreshed_at.max(access.fetched_at);
            }
        }
    }
    if let Ok(mut guard) = slot(region).lock() {
        if guard.is_none() {
            *guard = Some(restored);
        }
    }
    list(region)
}

pub fn store_public(region: Region, mut models: Vec<Value>) {
    for model in &mut models {
        model["workbuddyAccountScoped"] = json!(false);
    }
    let previous = list(region);
    for model in previous.models {
        if model.get("workbuddyAccountScoped").and_then(Value::as_bool) == Some(true)
            && !models.iter().any(|item| same_id(item, &model))
        {
            models.push(model);
        }
    }
    store(region, models);
}

pub fn merge_account(region: Region, models: &[Value]) {
    let mut list = list(region);
    merge_models(&mut list.models, models);
    store(region, list.models);
}

fn merge_models(current: &mut Vec<Value>, models: &[Value]) {
    for model in models {
        if current.iter().any(|item| same_id(item, model)) {
            continue;
        }
        let mut model = model.clone();
        model["workbuddyAccountScoped"] = json!(true);
        current.push(model);
    }
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

fn same_id(left: &Value, right: &Value) -> bool {
    left.get("id")
        .and_then(Value::as_str)
        .zip(right.get("id").and_then(Value::as_str))
        .is_some_and(|(left, right)| left.eq_ignore_ascii_case(right))
}
