//! 套餐预览与营销活动共用的发现结果；单一来源失败不清空另一来源。

use std::collections::BTreeMap;

use agent2api_provider_protocol::WireProxy;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use crate::errors::GatewayError;
use crate::logging;

use super::plans::{merge, ClaimablePlan};
use super::super::{report_activation, region::Region};
use super::{marketing, preview_billing, state};

pub struct PreviewSnapshot {
    pub plans: Vec<ClaimablePlan>,
    pub deployed: bool,
    pub activated: Option<bool>,
    pub activation_error: Option<String>,
    pub preview_error: Option<String>,
    pub marketing_error: Option<String>,
    pub claimed: BTreeMap<String, i64>,
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
        })
    }

    fn plan_json(&self, plan: &ClaimablePlan) -> Value {
        let claimed_at = self.claimed.get(&plan.plan_id).copied();
        json!({
            "planId": plan.plan_id,
            "name": plan.name,
            "description": plan.description,
            "priority": plan.priority,
            "startsAt": plan.starts_at,
            "endsAt": plan.ends_at,
            "claimed": claimed_at.is_some(),
            "claimedAt": claimed_at,
            "source": if plan.marketing_campaign_ids.is_empty() { "preview" } else { "marketing" },
            "entitlements": plan.entitlements.iter().map(|item| json!({
                "showName": item.show_name,
                "unitType": item.unit_type,
                "grantUnits": item.grant_units,
                "period": item.period,
                "effectiveAt": item.effective_at,
            })).collect::<Vec<_>>(),
        })
    }
}

pub async fn preview(
    ctx: &PluginCtx,
    region: Region,
    account: &str,
    jwt: &str,
    device_mid: Option<&str>,
    user_id: &str,
    proxy: Option<&WireProxy>,
    activate_empty: bool,
) -> Result<PreviewSnapshot, GatewayError> {
    let billing = preview_billing(region, jwt, device_mid, proxy).await;
    let marketing = marketing::query(region, jwt, device_mid, proxy).await;
    let mut snapshot = PreviewSnapshot {
        plans: Vec::new(), deployed: false, activated: None, activation_error: None,
        preview_error: None, marketing_error: None, claimed: BTreeMap::new(),
    };
    let mut succeeded = false;
    match billing {
        Ok(plans) => {
            succeeded = true;
            snapshot.deployed = plans.is_some();
            if let Some(plans) = plans { merge(&mut snapshot.plans, plans); }
        }
        Err(error) => snapshot.preview_error = Some(error.message),
    }
    match marketing {
        Ok(plans) => {
            succeeded = true;
            if !plans.is_empty() { snapshot.deployed = true; }
            merge(&mut snapshot.plans, plans);
        }
        Err(error) => snapshot.marketing_error = Some(error.message),
    }
    if !succeeded {
        return Err(GatewayError::with_status(502, format!(
            "套餐查询失败：{}；{}",
            snapshot.preview_error.as_deref().unwrap_or("套餐预览未完成"),
            snapshot.marketing_error.as_deref().unwrap_or("营销查询未完成"),
        )));
    }
    if activate_empty && snapshot.plans.is_empty() && snapshot.deployed && !user_id.trim().is_empty() {
        (snapshot.activated, snapshot.activation_error) = report_activation(region, user_id, device_mid, proxy).await;
        match preview_billing(region, jwt, device_mid, proxy).await {
            Ok(Some(plans)) => { merge(&mut snapshot.plans, plans); snapshot.preview_error = None; }
            Ok(None) => {}
            Err(error) => snapshot.preview_error = Some(error.message),
        }
        match marketing::query(region, jwt, device_mid, proxy).await {
            Ok(plans) => {
                if !plans.is_empty() { snapshot.deployed = true; }
                merge(&mut snapshot.plans, plans);
                snapshot.marketing_error = None;
            }
            Err(error) => snapshot.marketing_error = Some(error.message),
        }
    }
    if let Some(error) = &snapshot.preview_error {
        logging::log("[Claim]", &format!("ZCode 套餐预览未完成（保留营销结果）：{error}"));
    }
    if let Some(error) = &snapshot.marketing_error {
        logging::log("[Claim]", &format!("ZCode 营销查询未完成（保留预览结果）：{error}"));
    }
    if let Err(error) = state::remember_preview(ctx, account, jwt, device_mid, &snapshot.plans).await {
        logging::log("[Claim]", &format!("ZCode 套餐关联保存失败：{error}"));
        snapshot.marketing_error.get_or_insert(error.message);
    }
    snapshot.claimed = state::claimed_ids(ctx, account, jwt, device_mid).await;
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
        if let Some(plan) = state::known_plan(ctx, account, requested, jwt, device_mid).await { return Ok(Some(plan)); }
    }
    let snapshot = preview(ctx, region, account, jwt, device_mid, user_id, proxy, false).await?;
    if requested.is_empty() {
        Ok(snapshot.plans.into_iter().find(|plan| !snapshot.claimed.contains_key(&plan.plan_id)))
    } else {
        match snapshot.plans.into_iter().find(|plan| plan.plan_id == requested) {
            Some(plan) => Ok(Some(plan)),
            // 直接点名的旧调用仍由领取接口确认资格，不据此编造营销关联。
            None => Ok(Some(ClaimablePlan {
                plan_id: requested.to_string(), name: requested.to_string(), description: String::new(),
                priority: 0, starts_at: None, ends_at: None, entitlements: Vec::new(),
                marketing_campaign_ids: Vec::new(),
            })),
        }
    }
}
