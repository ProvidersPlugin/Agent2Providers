//! Trae 模型目录与按账号隔离的请求路由缓存。
//!
//! 批量获取 Agent、IDE 与旧 SOLO 通道；批量缺少的表分别补拉。
//! 对外模型和 function / model_name 路由来自同一次解析，刷新失败保留旧清单。
//! 磁盘上的对外目录由宿主管理；插件重启后的首个请求会补拉该账号的路由。

use std::collections::{HashMap, HashSet};
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use agent2api_provider_protocol::{
    RefreshOutcome as ModelRefreshOutcome, WireProxy as ResolvedProxy,
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::logging;

use super::catalog::{self, Catalog, Route, CHAT_FUNCTIONS, MAX_SUFFIX};
use super::credentials::Credential;
use super::headers::{solo_headers, HeaderIdentity, IDE_VERSION_CODE};
use super::http::post_json;
use super::{AGENT_BASE_URL, BATCH_MODELS_PATH, MODELS_PATH};

pub use super::catalog::{LEGACY_MAX_SUFFIX, LEGACY_SUFFIX};

const CACHE_TTL_MS: i64 = 60 * 60 * 1000;

#[derive(Clone, Default)]
struct Cache {
    catalog: Catalog,
    fetched_at: i64,
}

#[derive(Default)]
struct Caches {
    accounts: HashMap<String, Cache>,
    latest: String,
}

static SLOTS: OnceLock<RwLock<Caches>> = OnceLock::new();
static REFRESH_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn slot() -> &'static RwLock<Caches> {
    SLOTS.get_or_init(|| RwLock::new(Caches::default()))
}

fn key(credential: &Credential) -> String {
    if !credential.uid.trim().is_empty() {
        return credential.uid.trim().to_string();
    }
    use sha2::{Digest, Sha256};
    format!(
        "{:x}",
        Sha256::digest(credential.access_token.trim().as_bytes())
    )
}

fn snapshot_for(credential: &Credential) -> Cache {
    slot()
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .accounts
        .get(&key(credential))
        .cloned()
        .unwrap_or_default()
}

fn snapshot() -> Cache {
    let guard = slot().read().unwrap_or_else(|error| error.into_inner());
    guard
        .accounts
        .get(&guard.latest)
        .cloned()
        .unwrap_or_default()
}

pub fn list() -> Vec<Value> {
    snapshot().catalog.models
}

pub fn list_for(credential: &Credential) -> Vec<Value> {
    snapshot_for(credential).catalog.models
}

pub fn meta_for(credential: &Credential) -> agent2api_provider_protocol::CatalogMeta {
    let cached = snapshot_for(credential);
    agent2api_provider_protocol::CatalogMeta {
        remote: !cached.catalog.models.is_empty(),
        refreshed_at: cached.fetched_at,
    }
}

pub fn remote_refreshed() -> bool {
    !snapshot().catalog.models.is_empty()
}

pub fn last_refreshed_at() -> i64 {
    snapshot().fetched_at
}

/// 把历史 `<id>-solo` / `<id>-solo-max` 拆成基名与是否 Max。
/// 调用方必须先按原始 ID 精确查一次：上游真名恰好以 `-solo` 结尾时不能被改写。
pub(super) fn legacy_target(model: &str) -> Option<(&str, bool)> {
    if let Some(base) = strip_suffix(model, LEGACY_MAX_SUFFIX) {
        return Some((base, true));
    }
    strip_suffix(model, LEGACY_SUFFIX).map(|base| (base, false))
}

fn strip_suffix<'a>(name: &'a str, suffix: &str) -> Option<&'a str> {
    let cut = name.len().checked_sub(suffix.len())?;
    let base = name.get(..cut)?;
    let tail = name.get(cut..)?;
    tail.eq_ignore_ascii_case(suffix).then_some(base)
}

pub fn routes_for(credential: &Credential, model: &str) -> Vec<Route> {
    let guard = slot().read().unwrap_or_else(|error| error.into_inner());
    let Some(cached) = guard.accounts.get(&key(credential)) else {
        return Vec::new();
    };
    let name = model.trim().to_lowercase();
    if let Some(routes) = cached.catalog.routes.get(&name) {
        return routes.clone();
    }
    let Some((base, is_max)) = legacy_target(&name) else {
        return Vec::new();
    };
    if is_max {
        // 旧 `<id>-solo-max` 必须走这张表：`routes["<id>-max"]` 可能是另一条
        // 真实上游模型（Max ID 撞名时我们没有广告它）。
        if let Some(routes) = cached.catalog.legacy_max_routes.get(base) {
            return routes.clone();
        }
        return cached
            .catalog
            .routes
            .get(&format!("{base}{MAX_SUFFIX}"))
            .cloned()
            .unwrap_or_default();
    }
    cached
        .catalog
        .routes
        .get(base)
        .cloned()
        .unwrap_or_default()
}

pub fn remember_route(credential: &Credential, model: &str, route: &Route) {
    let mut guard = slot().write().unwrap_or_else(|error| error.into_inner());
    let Some(cached) = guard.accounts.get_mut(&key(credential)) else {
        return;
    };
    let name = model.trim().to_lowercase();
    if let Some(routes) = bucket_mut(&mut cached.catalog, &name) {
        if let Some(index) = routes.iter().position(|candidate| candidate == route) {
            let route = routes.remove(index);
            routes.insert(0, route);
        }
    }
}

fn bucket_mut<'a>(catalog: &'a mut Catalog, name: &str) -> Option<&'a mut Vec<Route>> {
    if catalog.routes.contains_key(name) {
        return catalog.routes.get_mut(name);
    }
    let (base, is_max) = legacy_target(name)?;
    if is_max {
        if let Some(routes) = catalog.legacy_max_routes.get_mut(base) {
            return Some(routes);
        }
        let max = format!("{base}{MAX_SUFFIX}");
        return catalog.routes.get_mut(&max);
    }
    catalog.routes.get_mut(base)
}

pub fn catalog_body(function: &str) -> Value {
    json!({
        "function": function,
        "config_names": Value::Null,
        "need_prompt": false,
        "current_config_info": Value::Null,
        "poly_prompt": true,
        "mode_type": Value::Null,
        "agent_type": Value::Null,
    })
}

pub fn batch_catalog_body() -> Value {
    json!({
        "functions": CHAT_FUNCTIONS,
        "agent_type": "",
        "current_config_info": { "config_name": "", "is_custom_model": false },
        "mode_type": 0,
        "access_type": 0,
        "ab_force_vids": "",
        "ab_autotest_advanced_mode": 0,
        "show_custom_model": true,
    })
}

pub async fn refresh(
    credential: &Credential,
    proxy: Option<&ResolvedProxy>,
    force: bool,
) -> ModelRefreshOutcome {
    if !credential.valid() {
        return ModelRefreshOutcome::unchanged();
    }
    if super::credentials::is_intl_variant(credential.variant()) {
        return ModelRefreshOutcome::failed(super::credentials::INTL_UNSUPPORTED);
    }
    let _refresh = REFRESH_LOCK.get_or_init(|| Mutex::new(())).lock().await;
    let current = snapshot_for(credential);
    if !force && current.fetched_at > 0 && logging::now_ms() - current.fetched_at < CACHE_TTL_MS {
        return ModelRefreshOutcome::unchanged();
    }
    let identity = HeaderIdentity {
        access_token: credential.access_token.trim(),
        uid: credential.uid.trim(),
        machine_id: credential.machine_id.trim(),
        device_id: credential.device_id.trim(),
    };
    let prepared: Vec<(String, String)> = solo_headers(&identity, false).into_iter().collect();
    let headers: Vec<(&str, String)> = prepared
        .iter()
        .map(|(name, value)| (name.as_str(), value.clone()))
        .collect();
    let catalog = match fetch_catalog(&headers, proxy).await {
        Ok(catalog) => catalog,
        Err(reason) => return ModelRefreshOutcome::failed(reason),
    };
    let count = catalog.models.len();
    let account = key(credential);
    {
        let mut guard = slot().write().unwrap_or_else(|error| error.into_inner());
        guard.accounts.insert(
            account.clone(),
            Cache {
                catalog,
                fetched_at: logging::now_ms(),
            },
        );
        guard.latest = account;
    }
    logging::log(
        "[Models]",
        &format!("Trae 模型目录已刷新（{count} 个模型，按模型选择聊天通道）"),
    );
    ModelRefreshOutcome::refreshed(count)
}

async fn request_catalog(
    path: &str,
    body: &Value,
    which: &str,
    headers: &[(&str, String)],
    proxy: Option<&ResolvedProxy>,
) -> Result<Value, (bool, String)> {
    let url = format!("{AGENT_BASE_URL}{path}");
    let reply = post_json(&url, body, headers, Duration::from_secs(20), proxy)
        .await
        .map_err(|error| (false, format!("{which}请求失败：{}", error.message)))?;
    if reply.status >= 300 {
        return Err((
            matches!(reply.status, 401 | 403 | 429),
            describe_failure(which, reply.status, &reply.body),
        ));
    }
    let payload = reply
        .json()
        .ok_or_else(|| (false, format!("{which}响应不是合法 JSON")))?;
    let error = payload
        .get("error")
        .filter(|value| value.is_object())
        .unwrap_or(&payload);
    let code = error
        .get("code")
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
        .unwrap_or(0);
    if code != 0 {
        let (_, message) = super::errors::error_event_fields(error);
        return Err((
            matches!(code, 1001 | 401 | 20101),
            format!("{which}业务错误 {code}：{message}"),
        ));
    }
    Ok(payload)
}

async fn fetch_catalog(
    headers: &[(&str, String)],
    proxy: Option<&ResolvedProxy>,
) -> Result<Catalog, String> {
    let mut reasons = Vec::new();
    let mut groups = match request_catalog(
        BATCH_MODELS_PATH,
        &batch_catalog_body(),
        "批量目录",
        headers,
        proxy,
    )
    .await
    {
        Ok(payload) => catalog::function_groups(&payload),
        Err((true, reason)) => return Err(reason),
        Err((false, reason)) => {
            reasons.push(reason);
            Vec::new()
        }
    };
    let listed: HashSet<String> = groups
        .iter()
        .filter_map(|group| {
            group
                .get("function")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    let missing: Vec<_> = CHAT_FUNCTIONS
        .into_iter()
        .filter(|function| !listed.contains(*function))
        .collect();
    let replies = futures::future::join_all(missing.iter().map(|function| {
        let body = catalog_body(function);
        async move {
            request_catalog(
                MODELS_PATH,
                &body,
                &format!("目录 {function}"),
                headers,
                proxy,
            )
            .await
        }
    }))
    .await;
    for (function, reply) in missing.into_iter().zip(replies) {
        match reply {
            Ok(payload) => match catalog::single_entries(&payload) {
                Some(entries) => {
                    groups.push(json!({ "function": function, "config_info_list": entries }))
                }
                None => reasons.push(format!("目录 {function} 缺少 config_info_list")),
            },
            Err((true, reason)) => return Err(reason),
            Err((false, reason)) => reasons.push(reason),
        }
    }
    let catalog = catalog::parse(&groups);
    if !catalog.models.is_empty() {
        return Ok(catalog);
    }
    reasons.push(format!(
        "各聊天通道均没有可选模型（IdeVersionCode={IDE_VERSION_CODE}）"
    ));
    Err(format!("未取到模型目录：{}", reasons.join("；")))
}

fn describe_failure(which: &str, status: u16, body: &str) -> String {
    let head: String = body.chars().take(120).collect();
    match status {
        401 | 403 => format!("{which} HTTP {status}（凭据可能已失效，请重新登录该账号）"),
        _ if head.is_empty() => format!("{which} HTTP {status}"),
        _ => format!("{which} HTTP {status}：{head}"),
    }
}
