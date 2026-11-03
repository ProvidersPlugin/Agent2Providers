//! 活动目录、账号预览与领取证据分别合并；匿名候选不扩大自动领取范围。

use std::collections::{BTreeMap, BTreeSet};

use agent2api_provider_protocol::WireProxy;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use crate::errors::GatewayError;
use crate::logging;

use super::plans::{merge, ClaimablePlan, OwnedPlan};
use super::super::{report_activation, region::Region};
use super::{catalog, marketing, ownership, preview_billing, state};

#[derive(Clone, Copy)]
pub enum PreviewPurpose {
    Display,
    Automatic,
}

pub struct PreviewSnapshot {
    pub plans: Vec<ClaimablePlan>,
    pub deployed: bool,
    pub activated: Option<bool>,
    pub activation_error: Option<String>,
    pub preview_error: Option<String>,
    pub marketing_error: Option<String>,
    pub catalog_error: Option<String>,
    pub ownership_error: Option<String>,
    pub ledger_error: Option<String>,
    pub claimed: BTreeMap<String, i64>,
    pub owned: BTreeMap<String, OwnedPlan>,
    pub ineligible: BTreeMap<String, String>,
    pub claim_periods: BTreeMap<String, (Option<i64>, Option<i64>)>,
    pub daily_checkin: Value,
    account_plan_ids: BTreeSet<String>,
    catalog_plan_ids: BTreeSet<String>,
    marketing_plan_ids: BTreeSet<String>,
}

impl PreviewSnapshot {
    pub fn to_json(&self) -> Value {
        json!({
            "plans": self.plans.iter().map(|plan| self.plan_json(plan)).collect::<Vec<_>>(),
            "deployed": self.deployed,
            "activated": self.activated,
            "activationError": self.activation_error,
            "previewError": self.preview_error,
            "marketingError": self.marketing_error,
            "catalogError": self.catalog_error,
            "ownershipError": self.ownership_error,
            "ledgerError": self.ledger_error,
            "dailyCheckin": self.daily_checkin,
        })
    }

    fn plan_json(&self, plan: &ClaimablePlan) -> Value {
        let claimed_at = self.claimed.get(&plan.plan_id).copied();
        let owned = self.owned.get(&plan.plan_id).filter(|owned| owned.matches_activity(plan));
        let claimed = claimed_at.is_some() || owned.is_some();
        let account_preview = self.account_plan_ids.contains(&plan.plan_id);
        let rejection = self.ineligible.get(&plan.plan_id);
        let claim_state = if claimed { "claimed" } else if rejection.is_some() { "ineligible" }
            else if account_preview { "claimable" } else { "unknown" };
        let mut sources = Vec::new();
        if account_preview { sources.push("preview"); }
        if self.marketing_plan_ids.contains(&plan.plan_id) { sources.push("marketing"); }
        if self.catalog_plan_ids.contains(&plan.plan_id) { sources.push("catalog"); }
        if sources.is_empty() { sources.push("ledger"); }
        let mut value = json!({
            "planId": plan.plan_id,
            "name": plan.name,
            "description": plan.description,
            "priority": plan.priority,
            "startsAt": plan.starts_at,
            "endsAt": plan.ends_at,
            "claimed": claimed,
            "claimedAt": claimed_at,
            "claimState": claim_state,
            "claimMessage": rejection,
            "source": sources[0],
            "sources": sources,
            "ownedPlan": owned.map(|owned| json!({
                "planId": owned.plan_id,
                "userPlanId": owned.user_plan_id,
                "name": owned.name,
                "state": owned.state,
                "startsAt": owned.starts_at,
                "endsAt": owned.ends_at,
                "pendingUntil": owned.pending_until,
                "entitlements": owned.entitlements.iter().map(entitlement_json).collect::<Vec<_>>(),
            })),
            "entitlements": plan.entitlements.iter().map(entitlement_json).collect::<Vec<_>>(),
        });
        if let Some((starts, ends)) = self.claim_periods.get(&plan.plan_id) {
            value["claimStartsAt"] = json!(starts);
            value["claimEndsAt"] = json!(ends);
        }
        value
    }

    fn add_account_plans(&mut self, plans: Vec<ClaimablePlan>, marketing: bool) {
        for plan in &plans {
            if marketing { self.marketing_plan_ids.insert(plan.plan_id.clone()); }
            else { self.account_plan_ids.insert(plan.plan_id.clone()); }
        }
        if !plans.is_empty() { self.deployed = true; }
        merge(&mut self.plans, plans);
    }
}

fn entitlement_json(item: &super::plans::PlanEntitlement) -> Value {
    json!({
        "showName": item.show_name, "unitType": item.unit_type,
        "grantUnits": item.grant_units, "period": item.period, "effectiveAt": item.effective_at,
    })
}

pub async fn preview(
    ctx: &PluginCtx,
    region: Region,
    account: &str,
    jwt: &str,
    device_mid: Option<&str>,
    user_id: &str,
    proxy: Option<&WireProxy>,
    purpose: PreviewPurpose,
    activate_empty: bool,
) -> Result<PreviewSnapshot, GatewayError> {
    let (billing, marketing) = tokio::join!(
        preview_billing(region, jwt, device_mid, proxy),
        marketing::query(region, jwt, device_mid, proxy),
    );
    let mut snapshot = PreviewSnapshot {
        plans: Vec::new(), deployed: false, activated: None, activation_error: None,
        preview_error: None, marketing_error: None, catalog_error: None, ownership_error: None,
        ledger_error: None, claimed: BTreeMap::new(), owned: BTreeMap::new(), ineligible: BTreeMap::new(),
        claim_periods: BTreeMap::new(), daily_checkin: Value::Null,
        account_plan_ids: BTreeSet::new(), catalog_plan_ids: BTreeSet::new(), marketing_plan_ids: BTreeSet::new(),
    };
    let mut succeeded = false;
    match billing {
        Ok(plans) => {
            succeeded = true;
            snapshot.deployed = plans.is_some();
            if let Some(plans) = plans { snapshot.add_account_plans(plans, false); }
        }
        Err(error) => snapshot.preview_error = Some(error.message),
    }
    match marketing {
        Ok(plans) => {
            succeeded = true;
            snapshot.add_account_plans(plans, true);
        }
        Err(error) => snapshot.marketing_error = Some(error.message),
    }
    if activate_empty && snapshot.plans.is_empty() && snapshot.deployed && !user_id.trim().is_empty() {
        (snapshot.activated, snapshot.activation_error) = report_activation(region, user_id, device_mid, proxy).await;
        let (billing, marketing) = tokio::join!(
            preview_billing(region, jwt, device_mid, proxy),
            marketing::query(region, jwt, device_mid, proxy),
        );
        match billing {
            Ok(Some(plans)) => {
                succeeded = true;
                snapshot.add_account_plans(plans, false);
                snapshot.preview_error = None;
            }
            Ok(None) => {}
            Err(error) => snapshot.preview_error = Some(error.message),
        }
        match marketing {
            Ok(plans) => {
                succeeded = true;
                snapshot.add_account_plans(plans, true);
                snapshot.marketing_error = None;
            }
            Err(error) => snapshot.marketing_error = Some(error.message),
        }
    }
    let saved = match state::preview_state(ctx, region, account, jwt, device_mid, user_id).await {
        Ok(saved) => saved,
        Err(error) => { snapshot.ledger_error = Some(error.message); state::PreviewState::default() }
    };
    snapshot.claimed = saved.claimed;
    snapshot.owned = saved.owned;
    snapshot.ineligible = saved.ineligible;
    snapshot.claim_periods = saved.claim_periods;
    let checkin_plan_ids = snapshot.account_plan_ids.union(&snapshot.marketing_plan_ids)
        .cloned().collect::<BTreeSet<_>>();
    merge(&mut snapshot.plans, saved.checkin_plans);
    let saved_plans = saved.plans;
    if matches!(purpose, PreviewPurpose::Display) {
        match catalog::query(region, proxy).await {
            Ok(plans) => {
                succeeded = true;
                if let Some(plans) = plans {
                    if !plans.is_empty() { snapshot.deployed = true; }
                    snapshot.catalog_plan_ids.extend(plans.iter().map(|plan| plan.plan_id.clone()));
                    merge(&mut snapshot.plans, plans);
                }
            }
            Err(error) => snapshot.catalog_error = Some(error.message),
        }
        merge(&mut snapshot.plans, saved_plans.clone());
        snapshot.claimed.retain(|id, at| {
            snapshot.plans.iter().find(|plan| &plan.plan_id == id).is_none_or(|plan| {
                match (plan.starts_at, snapshot.claim_periods.get(id).and_then(|(_, end)| *end)
                    .or_else(|| saved_plans.iter().find(|saved| &saved.plan_id == id).and_then(|saved| saved.ends_at)))
                {
                    (Some(start), Some(end)) => end > start,
                    (Some(start), None) => start <= *at / 1000,
                    _ => true,
                }
            })
        });
        let needs_ownership = snapshot.plans.iter().any(|plan| {
            !snapshot.owned.get(&plan.plan_id).is_some_and(|owned| owned.matches_activity(plan))
        });
        if needs_ownership && !jwt.trim().is_empty() {
            match ownership::query(region, account, jwt, device_mid, proxy).await {
                Ok(plans) => {
                    for plan in &snapshot.plans {
                        if let Some(owned) = plans.iter().filter(|owned| owned.matches_activity(plan))
                            .max_by_key(|owned| owned.starts_at)
                        {
                            snapshot.owned.insert(plan.plan_id.clone(), owned.clone());
                        }
                    }
                }
                Err(error) => snapshot.ownership_error = Some(error.message),
            }
        }
    }
    match state::remember_preview(
        ctx, region, account, jwt, device_mid, user_id, &snapshot.plans, &snapshot.owned,
        &checkin_plan_ids, snapshot.preview_error.is_none() && snapshot.marketing_error.is_none()
            && snapshot.catalog_error.is_none() && snapshot.ownership_error.is_none()
            && snapshot.ledger_error.is_none(),
    ).await {
        Ok(daily_checkin) => snapshot.daily_checkin = daily_checkin,
        Err(error) => snapshot.ledger_error = Some(error.message),
    }
    for (source, error) in [
        ("账号预览", &snapshot.preview_error), ("营销活动", &snapshot.marketing_error),
        ("匿名目录", &snapshot.catalog_error), ("账号套餐", &snapshot.ownership_error),
        ("领取台账", &snapshot.ledger_error),
    ] {
        if let Some(error) = error { logging::log("[Claim]", &format!("ZCode {source}未完成：{error}")); }
    }
    if !succeeded && snapshot.plans.is_empty() {
        return Err(GatewayError::with_status(502, format!(
            "套餐查询失败：{}",
            [snapshot.preview_error.as_deref(), snapshot.marketing_error.as_deref(), snapshot.catalog_error.as_deref()]
                .into_iter().flatten().collect::<Vec<_>>().join("；"),
        )));
    }
    Ok(snapshot)
}

pub(crate) async fn resolve_target(
    ctx: &PluginCtx,
    region: Region,
    account: &str,
    jwt: &str,
    device_mid: Option<&str>,
    user_id: &str,
    proxy: Option<&WireProxy>,
    requested: &str,
) -> Result<Option<ClaimablePlan>, GatewayError> {
    let requested = requested.trim();
    if !requested.is_empty() {
        if super::plans::identifier(&Value::String(requested.to_string())).is_none() {
            return Err(GatewayError::with_status(400, "套餐标识无效"));
        }
        if let Some(plan) = state::known_plan(ctx, region, account, requested, jwt, device_mid, user_id).await {
            return Ok(Some(plan));
        }
    }
    let snapshot = preview(
        ctx, region, account, jwt, device_mid, user_id, proxy, PreviewPurpose::Automatic, false,
    ).await;
    if !requested.is_empty() {
        let plan = snapshot.ok().and_then(|snapshot| snapshot.plans.into_iter().find(|plan| plan.plan_id == requested));
        // 点名领取不依赖预览成功；上游 claim 才是资格的最终判据。
        return Ok(Some(plan.unwrap_or_else(|| ClaimablePlan {
            plan_id: requested.to_string(), name: requested.to_string(), description: String::new(),
            priority: 0, starts_at: None, ends_at: None, entitlements: Vec::new(),
            marketing_campaign_ids: Vec::new(),
        })));
    }
    let snapshot = snapshot?;
    Ok(snapshot.plans.into_iter().find(|plan| {
        !snapshot.claimed.contains_key(&plan.plan_id)
            && !snapshot.owned.get(&plan.plan_id).is_some_and(|owned| owned.matches_activity(plan))
    }))
}
