//! 匿名预览只提供活动目录，不携带账号身份，也不确认领取资格。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use agent2api_provider_protocol::WireProxy;

use crate::errors::GatewayError;
use crate::flight::{Join, Table};

use super::plans::ClaimablePlan;
use super::super::{credentials, region::Region};
use super::{app_version, platform, preview_billing_with_version};

const CATALOG_TTL: Duration = Duration::from_secs(60);
const EMPTY_TTL: Duration = Duration::from_secs(5);
const MAX_SCOPES: usize = 64;

type Catalog = Option<Vec<ClaimablePlan>>;

struct CachedCatalog {
    expires: Instant,
    plans: Catalog,
}

fn cache() -> &'static Mutex<HashMap<String, CachedCatalog>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedCatalog>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn flights() -> &'static Table<Catalog> {
    static FLIGHTS: OnceLock<Table<Catalog>> = OnceLock::new();
    FLIGHTS.get_or_init(Table::new)
}

fn cached(key: &str) -> Option<Catalog> {
    let mut entries = cache().lock().ok()?;
    entries.retain(|_, entry| entry.expires > Instant::now());
    entries.get(key).map(|entry| entry.plans.clone())
}

pub(super) async fn query(
    region: Region,
    proxy: Option<&WireProxy>,
) -> Result<Catalog, GatewayError> {
    let version = app_version();
    let route = proxy.map(WireProxy::to_url).unwrap_or_else(|| "direct".to_string());
    let key = crate::flight::fingerprint(&format!(
        "{}:{version}:{}:{route}", region.provider_id(), platform(),
    ));
    if let Some(plans) = cached(&key) { return Ok(plans); }
    match flights().join(&key) {
        Join::Waiter(waiter) => waiter.wait().await,
        Join::Leader(guard) => {
            let result = match cached(&key) {
                Some(plans) => Ok(plans),
                None => match credentials::new_uuid() {
                    Some(mid) => preview_billing_with_version(region, "", Some(&mid), proxy, &version).await,
                    None => Err(GatewayError::with_status(502, "无法生成匿名活动探测标识")),
                },
            };
            if let Ok(plans) = &result {
                let ttl = if plans.as_ref().is_some_and(|plans| !plans.is_empty()) {
                    CATALOG_TTL
                } else {
                    EMPTY_TTL
                };
                if let Ok(mut entries) = cache().lock() {
                    entries.retain(|_, entry| entry.expires > Instant::now());
                    if entries.len() >= MAX_SCOPES {
                        if let Some(oldest) = entries.iter().min_by_key(|(_, entry)| entry.expires)
                            .map(|(key, _)| key.clone())
                        {
                            entries.remove(&oldest);
                        }
                    }
                    entries.insert(key.clone(), CachedCatalog {
                        expires: Instant::now() + ttl, plans: plans.clone(),
                    });
                }
            }
            guard.finish(result.clone());
            result
        }
    }
}
