//! 领取状态仅由账号名下套餐确认；不使用编码套餐监控通道的空列表。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use agent2api_provider_protocol::WireProxy;
use serde_json::Value;

use crate::errors::GatewayError;
use crate::flight::{Join, Table};

use super::plans::{integer, OwnedPlan, PlanEntitlement};
use super::super::{balance, region::Region};
use super::{app_version, platform};

const CACHE_TTL: Duration = Duration::from_secs(15);
const MAX_SCOPES: usize = 128;

struct CachedPlans {
    expires: Instant,
    plans: Vec<OwnedPlan>,
}

fn cache() -> &'static Mutex<HashMap<String, CachedPlans>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedPlans>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn flights() -> &'static Table<Vec<OwnedPlan>> {
    static FLIGHTS: OnceLock<Table<Vec<OwnedPlan>>> = OnceLock::new();
    FLIGHTS.get_or_init(Table::new)
}

fn scope(region: Region, account: &str, jwt: &str, mid: Option<&str>, proxy: Option<&WireProxy>) -> String {
    let route = proxy.map(WireProxy::to_url).unwrap_or_else(|| "direct".to_string());
    crate::flight::fingerprint(&format!(
        "{}:{account}:{}:{}:{jwt}:{}:{route}",
        region.provider_id(), app_version(), platform(), mid.unwrap_or(""),
    ))
}

fn cached(key: &str) -> Option<Vec<OwnedPlan>> {
    let mut entries = cache().lock().ok()?;
    entries.retain(|_, entry| entry.expires > Instant::now());
    entries.get(key).map(|entry| entry.plans.clone())
}

pub(super) fn invalidate(region: Region, account: &str, jwt: &str, mid: Option<&str>, proxy: Option<&WireProxy>) {
    if let Ok(mut entries) = cache().lock() { entries.remove(&scope(region, account, jwt, mid, proxy)); }
}

pub(super) async fn query(
    region: Region,
    account: &str,
    jwt: &str,
    mid: Option<&str>,
    proxy: Option<&WireProxy>,
) -> Result<Vec<OwnedPlan>, GatewayError> {
    let key = scope(region, account, jwt, mid, proxy);
    if let Some(plans) = cached(&key) { return Ok(plans); }
    match flights().join(&key) {
        Join::Waiter(waiter) => waiter.wait().await,
        Join::Leader(guard) => {
            let result = match cached(&key) {
                Some(plans) => Ok(plans),
                None => balance::billing_channel(region, jwt, mid, proxy).await.map(|value| parse(&value)),
            };
            if let Ok(plans) = &result {
                if let Ok(mut entries) = cache().lock() {
                    entries.retain(|_, entry| entry.expires > Instant::now());
                    if entries.len() >= MAX_SCOPES {
                        if let Some(oldest) = entries.iter().min_by_key(|(_, entry)| entry.expires)
                            .map(|(key, _)| key.clone())
                        {
                            entries.remove(&oldest);
                        }
                    }
                    entries.insert(key.clone(), CachedPlans {
                        expires: Instant::now() + CACHE_TTL, plans: plans.clone(),
                    });
                }
            }
            guard.finish(result.clone());
            result
        }
    }
}

fn parse(value: &Value) -> Vec<OwnedPlan> {
    value.get("plans").and_then(Value::as_array).into_iter().flatten().filter_map(|plan| {
        let plan_id = plan.get("planId").and_then(Value::as_str)?.trim();
        if plan_id.is_empty() { return None; }
        let entitlements = plan.get("entitlements").and_then(Value::as_array)
            .into_iter().flatten().filter_map(|item| {
                Some(PlanEntitlement {
                    show_name: item.get("showName").and_then(Value::as_str)?.to_string(),
                    unit_type: text(item, "unitType"),
                    grant_units: item.get("grantUnits").and_then(integer).unwrap_or(0),
                    period: text(item, "period"),
                    effective_at: item.get("effectiveAt").and_then(integer),
                })
            }).collect();
        Some(OwnedPlan {
            plan_id: plan_id.to_string(),
            user_plan_id: plan.get("userPlanId").and_then(Value::as_str).map(str::to_string),
            name: text(plan, "name"),
            state: text(plan, "state"),
            starts_at: plan.get("startsAt").and_then(integer),
            ends_at: plan.get("endsAt").and_then(integer),
            pending_until: plan.get("pendingUntil").and_then(integer),
            entitlements,
        })
    }).collect()
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).map(str::trim).unwrap_or("").to_string()
}
