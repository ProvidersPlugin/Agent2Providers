//! 已验证的套餐与营销关联按账号持久化；回执目标不接受前端输入。

use std::collections::BTreeMap;

use agent2api_provider_protocol::WireProxy;
use agent2api_provider_sdk::PluginCtx;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::GatewayError;
use crate::logging;

use super::plans::{identifier, ClaimablePlan};
use super::super::{captcha, region::Region};
use super::{claim_upstream, marketing, ClaimFailure, ClaimOutcome};

const LEDGER_KIND: &str = "zcodeClaimPlans";
const MAX_SAVED_PLANS: usize = 256;
const RETENTION_MS: i64 = 90 * 86_400_000;
const PREVIEW_TTL_MS: i64 = 15 * 60_000;

#[derive(Clone, Default, Deserialize, Serialize)]
struct Ledger {
    #[serde(default)]
    identity: String,
    #[serde(default)]
    plans: BTreeMap<String, SavedPlan>,
}

#[derive(Clone, Deserialize, Serialize)]
struct SavedPlan {
    plan: ClaimablePlan,
    seen_at: i64,
    #[serde(default)]
    claimed_at: Option<i64>,
    #[serde(default)]
    confirmed_campaigns: Vec<String>,
}

#[derive(Clone)]
pub struct ClaimAttempt {
    pub outcome: ClaimOutcome,
    pub marketing_error: Option<String>,
}

pub(super) async fn claimed_ids(ctx: &PluginCtx, account: &str, jwt: &str, device_mid: Option<&str>) -> BTreeMap<String, i64> {
    match load(ctx, account).await {
        Ok(mut ledger) => {
            bind_identity(&mut ledger, jwt, device_mid);
            ledger.plans.into_iter().filter_map(|(id, saved)| saved.claimed_at.map(|at| (id, at))).collect()
        }
        Err(_) => BTreeMap::new(),
    }
}

pub(super) async fn remember_preview(
    ctx: &PluginCtx,
    account: &str,
    jwt: &str,
    device_mid: Option<&str>,
    plans: &[ClaimablePlan],
) -> Result<(), GatewayError> {
    let key = format!("zcode-claims:{account}");
    crate::flight::run(&key, || async {
        let mut ledger = load(ctx, account).await?;
        bind_identity(&mut ledger, jwt, device_mid);
        let now = crate::http::now_ms();
        prune(&mut ledger, now);
        for plan in plans {
            match ledger.plans.get_mut(&plan.plan_id) {
                Some(saved) => {
                    let mut refreshed = plan.clone();
                    for campaign in &saved.plan.marketing_campaign_ids {
                        if !refreshed.marketing_campaign_ids.contains(campaign) {
                            refreshed.marketing_campaign_ids.push(campaign.clone());
                        }
                    }
                    saved.plan = refreshed;
                    saved.seen_at = now;
                }
                None => {
                    ledger.plans.insert(plan.plan_id.clone(), SavedPlan {
                        plan: plan.clone(), seen_at: now, claimed_at: None,
                        confirmed_campaigns: Vec::new(),
                    });
                }
            }
        }
        prune(&mut ledger, now);
        save(ctx, account, &ledger).await.map_err(GatewayError::into_message)
    }).await.map_err(GatewayError::from)
}

pub(super) async fn known_plan(
    ctx: &PluginCtx,
    account: &str,
    plan_id: &str,
    jwt: &str,
    device_mid: Option<&str>,
) -> Option<ClaimablePlan> {
    let mut ledger = load(ctx, account).await.ok()?;
    bind_identity(&mut ledger, jwt, device_mid);
    let saved = ledger.plans.remove(plan_id)?;
    if saved.claimed_at.is_some()
        || crate::http::now_ms().saturating_sub(saved.seen_at) <= PREVIEW_TTL_MS
    {
        Some(saved.plan)
    } else {
        None
    }
}

pub async fn claim_plan(
    ctx: &PluginCtx,
    region: Region,
    account: &str,
    jwt: &str,
    device_mid: Option<&str>,
    proxy: Option<&WireProxy>,
    plan: &ClaimablePlan,
    captcha: Option<(&str, Option<&str>)>,
) -> Result<ClaimAttempt, GatewayError> {
    let key = format!("zcode-claims:{account}");
    crate::flight::run(&key, || async {
        let mut ledger = load(ctx, account).await?;
        bind_identity(&mut ledger, jwt, device_mid);
        prune(&mut ledger, crate::http::now_ms());
        let saved = ledger.plans.entry(plan.plan_id.clone()).or_insert_with(|| SavedPlan {
            plan: plan.clone(), seen_at: crate::http::now_ms(), claimed_at: None,
            confirmed_campaigns: Vec::new(),
        });
        super::plans::merge_plan(&mut saved.plan, plan.clone());
        let outcome = if saved.claimed_at.is_some() {
            ClaimOutcome::Failed {
                plan_id: plan.plan_id.clone(), failure: ClaimFailure::AlreadyClaimed,
                code: 1003, message: "该套餐已领取，不重复提交".to_string(),
                failure_ends_at: saved.plan.ends_at,
            }
        } else {
            let pooled = if captcha.is_none() && captcha::required() { captcha::take() } else { None };
            let (param, region_tag) = captcha.unwrap_or_else(|| {
                pooled.as_ref().map(|(param, region)| {
                    (param.as_str(), (!region.is_empty()).then_some(region.as_str()))
                }).unwrap_or(("", None))
            });
            claim_upstream(region, jwt, &plan.plan_id, param, region_tag, device_mid, proxy).await?
        };
        if matches!(&outcome, ClaimOutcome::Failed { failure: ClaimFailure::Captcha, .. }) {
            captcha::note_challenge();
        }
        let settled = matches!(&outcome,
            ClaimOutcome::Claimed { .. }
            | ClaimOutcome::Failed { failure: ClaimFailure::AlreadyClaimed, code: 1003, .. }
        );
        let mut marketing_error = None;
        if settled {
            let saved = ledger.plans.get_mut(&plan.plan_id)
                .ok_or_else(|| GatewayError::with_status(502, "领取台账缺少目标套餐"))?;
            saved.claimed_at.get_or_insert_with(crate::http::now_ms);
            if let Err(error) = save(ctx, account, &ledger).await {
                logging::log("[Claim]", &format!("ZCode 套餐领取结果落盘失败（不影响已领取结果）：{error}"));
                marketing_error = Some(error.message);
            }
            let campaigns = ledger.plans.get(&plan.plan_id).map(|saved| {
                saved.plan.marketing_campaign_ids.iter()
                    .filter(|id| !saved.confirmed_campaigns.contains(id)).cloned().collect::<Vec<_>>()
            }).unwrap_or_default();
            for campaign in campaigns {
                match marketing::confirm(region, jwt, device_mid, proxy, &campaign).await {
                    Ok(()) => {
                        if let Some(saved) = ledger.plans.get_mut(&plan.plan_id) {
                            saved.confirmed_campaigns.push(campaign);
                        }
                        if let Err(error) = save(ctx, account, &ledger).await {
                            logging::log("[Claim]", &format!("ZCode 营销回执状态落盘失败：{error}"));
                            marketing_error = Some(error.message);
                        }
                    }
                    Err(error) => {
                        logging::log("[Claim]", &format!("ZCode 营销回执未完成（套餐已领取）：{error}"));
                        marketing_error = Some(error);
                    }
                }
            }
        }
        Ok(ClaimAttempt { outcome, marketing_error })
    }).await.map_err(GatewayError::from)
}

fn bind_identity(ledger: &mut Ledger, jwt: &str, device_mid: Option<&str>) {
    let identity = crate::flight::fingerprint(&format!("{}:{}", jwt.trim(), device_mid.unwrap_or("")));
    if ledger.identity != identity {
        ledger.plans.clear();
        ledger.identity = identity;
    }
}

async fn load(ctx: &PluginCtx, account: &str) -> Result<Ledger, GatewayError> {
    let value = ctx.ledger_get(account, LEDGER_KIND).await
        .map_err(|error| GatewayError::with_status(502, format!("读取套餐台账失败：{error}")))?;
    if value.is_null() { return Ok(Ledger::default()); }
    let ledger: Ledger = serde_json::from_value(value)
        .map_err(|_| GatewayError::with_status(502, "套餐台账格式异常，未发送领取或回执"))?;
    if ledger.plans.iter().any(|(id, saved)| {
        identifier(&Value::String(id.clone())).is_none() || saved.plan.plan_id != *id
            || saved.plan.marketing_campaign_ids.iter().any(|id| identifier(&Value::String(id.clone())).is_none())
    }) {
        return Err(GatewayError::with_status(502, "套餐台账关联异常，未发送领取或回执"));
    }
    Ok(ledger)
}

async fn save(ctx: &PluginCtx, account: &str, ledger: &Ledger) -> Result<(), GatewayError> {
    let value = serde_json::to_value(ledger)
        .map_err(|_| GatewayError::with_status(502, "套餐台账无法序列化"))?;
    ctx.ledger_put(account, LEDGER_KIND, &value).await
        .map_err(|error| GatewayError::with_status(502, format!("保存套餐台账失败：{error}")))
}

fn prune(ledger: &mut Ledger, now: i64) {
    ledger.plans.retain(|_, saved| now.saturating_sub(saved.seen_at) <= RETENTION_MS
        || saved.claimed_at.is_some_and(|at| now.saturating_sub(at) <= RETENTION_MS));
    while ledger.plans.len() > MAX_SAVED_PLANS {
        let oldest = ledger.plans.iter().min_by_key(|(_, saved)| saved.seen_at)
            .map(|(id, _)| id.clone());
        match oldest { Some(id) => { ledger.plans.remove(&id); }, None => break }
    }
}
