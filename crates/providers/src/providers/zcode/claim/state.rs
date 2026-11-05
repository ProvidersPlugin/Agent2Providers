//! 已验证的套餐与营销关联按账号持久化；回执目标不接受前端输入。

use std::collections::BTreeMap;

use agent2api_provider_protocol::WireProxy;
use agent2api_provider_sdk::PluginCtx;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::GatewayError;
use crate::logging;

use super::plans::{identifier, ClaimablePlan, OwnedPlan};
use super::super::{claim_verification, region::Region};
use super::{app_version, claim_upstream, marketing, ownership, ClaimFailure, ClaimOutcome};

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
    #[serde(default)]
    checkin_targets: BTreeMap<String, i64>,
    #[serde(default)]
    checkin_sources_at: Option<i64>,
    #[serde(default)]
    checkin_sources_complete: bool,
    #[serde(default)]
    checkin_verification: Option<VerificationRequired>,
}

#[derive(Clone, Deserialize, Serialize)]
struct VerificationRequired {
    at: i64,
    plan_id: String,
    message: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct SavedPlan {
    plan: ClaimablePlan,
    seen_at: i64,
    #[serde(default)]
    claimed_at: Option<i64>,
    #[serde(default)]
    claim_starts_at: Option<i64>,
    #[serde(default)]
    claim_ends_at: Option<i64>,
    #[serde(default)]
    owned: Option<OwnedPlan>,
    #[serde(default)]
    rejection: Option<Rejection>,
    #[serde(default)]
    confirmed_campaigns: Vec<String>,
}

impl SavedPlan {
    fn new(plan: &ClaimablePlan) -> Self {
        Self {
            plan: plan.clone(), seen_at: crate::http::now_ms(), claimed_at: None,
            claim_starts_at: None, claim_ends_at: None, owned: None, rejection: None,
            confirmed_campaigns: Vec::new(),
        }
    }

    fn claimed_for(&self, plan: &ClaimablePlan) -> bool {
        self.claimed_for_at(plan, crate::http::now_ms())
    }

    fn claimed_for_at(&self, plan: &ClaimablePlan, now: i64) -> bool {
        if self.claimed_at.is_none() {
            return self.owned.as_ref().is_some_and(|owned| owned.matches_activity(plan));
        }
        let Some(at) = self.claimed_at else { return false; };
        let end = self.claim_ends_at.or(self.plan.ends_at);
        if end.is_some_and(|end| end <= now / 1000) { return false; }
        match (plan.starts_at, end) {
            (Some(start), Some(end)) => end > start,
            _ => !plan.starts_at.is_some_and(|start| start > self.claim_starts_at.unwrap_or(at / 1000)),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct Rejection {
    version: String,
    message: String,
    at: i64,
}

#[derive(Default)]
pub(super) struct PreviewState {
    pub plans: Vec<ClaimablePlan>,
    pub claimed: BTreeMap<String, i64>,
    pub owned: BTreeMap<String, OwnedPlan>,
    pub ineligible: BTreeMap<String, String>,
    pub claim_periods: BTreeMap<String, (Option<i64>, Option<i64>)>,
    pub checkin_plans: Vec<ClaimablePlan>,
}

#[derive(Clone)]
pub struct ClaimAttempt {
    pub outcome: ClaimOutcome,
    pub marketing_error: Option<String>,
    pub daily_checkin: Value,
    pub claimed_at: Option<i64>,
}

pub(super) async fn preview_state(
    ctx: &PluginCtx,
    region: Region,
    account: &str,
    jwt: &str,
    device_mid: Option<&str>,
    user_id: &str,
) -> Result<PreviewState, GatewayError> {
    let mut ledger = load(ctx, account).await?;
    bind_identity(&mut ledger, region, account, jwt, device_mid, user_id);
    let now = crate::http::now_ms();
    prune(&mut ledger, now);
    let mut state = PreviewState::default();
    for (id, saved) in ledger.plans {
        if ledger.checkin_targets.get(&id).is_some_and(|at| same_day(*at, now))
            || saved.claimed_at.is_some_and(|at| same_day(at, now)) && saved.claimed_for(&saved.plan)
        {
            state.checkin_plans.push(saved.plan.clone());
        }
        if saved.claimed_for(&saved.plan) {
            if let Some(at) = saved.claimed_at { state.claimed.insert(id.clone(), at); }
            state.claim_periods.insert(id.clone(), (saved.claim_starts_at, saved.claim_ends_at));
            if let Some(owned) = saved.owned.filter(|owned| owned.matches_activity(&saved.plan)) {
                state.owned.insert(id.clone(), owned);
            }
            let recent = saved.claimed_at.is_some_and(|at| now.saturating_sub(at) <= 86_400_000);
            if saved.claim_ends_at.or(saved.plan.ends_at).is_some_and(|end| end > now / 1000)
                || state.owned.contains_key(&id) || recent
            {
                state.plans.push(saved.plan.clone());
            }
        }
        if let Some(rejection) = saved.rejection.filter(|rejection| {
            rejection.version == app_version() && now.saturating_sub(rejection.at) <= PREVIEW_TTL_MS
        }) {
            state.ineligible.insert(id, rejection.message);
        }
    }
    Ok(state)
}

pub(super) async fn remember_preview(
    ctx: &PluginCtx,
    region: Region,
    account: &str,
    jwt: &str,
    device_mid: Option<&str>,
    user_id: &str,
    plans: &[ClaimablePlan],
    owned: &BTreeMap<String, OwnedPlan>,
    checkin_plan_ids: &std::collections::BTreeSet<String>,
    sources_complete: bool,
) -> Result<Value, GatewayError> {
    let key = format!("zcode-claims:{}:{account}", region.provider_id());
    crate::flight::run(&key, || async {
        let mut ledger = load(ctx, account).await?;
        bind_identity(&mut ledger, region, account, jwt, device_mid, user_id);
        let now = crate::http::now_ms();
        prune(&mut ledger, now);
        let current_sources = same_day(ledger.checkin_sources_at.unwrap_or(0), now);
        if !current_sources { ledger.checkin_targets.clear(); }
        ledger.checkin_sources_complete = sources_complete
            || current_sources && ledger.checkin_sources_complete;
        ledger.checkin_sources_at = Some(now);
        for plan_id in checkin_plan_ids {
            ledger.checkin_targets.insert(plan_id.clone(), now);
        }
        for plan in plans {
            let saved = ledger.plans.entry(plan.plan_id.clone()).or_insert_with(|| SavedPlan::new(plan));
            let mut refreshed = plan.clone();
            super::plans::merge_plan(&mut refreshed, saved.plan.clone());
            if !saved.claimed_for(&refreshed) {
                saved.claimed_at = None;
                saved.claim_starts_at = None;
                saved.claim_ends_at = None;
                saved.owned = None;
            }
            saved.plan = refreshed;
            saved.seen_at = now;
            if let Some(owned) = owned.get(&plan.plan_id).filter(|owned| owned.matches_activity(plan)) {
                saved.owned = Some(owned.clone());
                saved.rejection = None;
            }
        }
        for (plan_id, saved) in &ledger.plans {
            if saved.claimed_at.is_some_and(|at| same_day(at, now)) && saved.claimed_for_at(&saved.plan, now)
                || saved.owned.as_ref().is_some_and(|owned| {
                    owned.starts_at.is_some_and(|at| same_day(at.saturating_mul(1000), now))
                        && owned.matches_activity(&saved.plan)
                })
            {
                ledger.checkin_targets.entry(plan_id.clone()).or_insert(now);
            }
        }
        prune(&mut ledger, now);
        save(ctx, account, &ledger).await.map_err(GatewayError::into_message)?;
        Ok(daily_checkin_view(&ledger, crate::http::now_ms()))
    }).await.map_err(GatewayError::from)
}

pub(super) async fn known_plan(
    ctx: &PluginCtx,
    region: Region,
    account: &str,
    plan_id: &str,
    jwt: &str,
    device_mid: Option<&str>,
    user_id: &str,
) -> Option<ClaimablePlan> {
    let mut ledger = load(ctx, account).await.ok()?;
    bind_identity(&mut ledger, region, account, jwt, device_mid, user_id);
    let saved = ledger.plans.get(plan_id)?;
    if saved.claimed_for(&saved.plan)
        || crate::http::now_ms().saturating_sub(saved.seen_at) <= PREVIEW_TTL_MS
    {
        Some(saved.plan.clone())
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
    user_id: &str,
    proxy: Option<&WireProxy>,
    plan: &ClaimablePlan,
    captcha: Option<(&str, Option<&str>)>,
    deadline: Option<tokio::time::Instant>,
) -> Result<ClaimAttempt, GatewayError> {
    let key = format!("zcode-claims:{}:{account}", region.provider_id());
    crate::flight::run(&key, || async {
        let mut ledger = load(ctx, account).await?;
        bind_identity(&mut ledger, region, account, jwt, device_mid, user_id);
        let now = crate::http::now_ms();
        prune(&mut ledger, now);
        for (plan_id, saved) in &ledger.plans {
            if saved.claimed_at.is_some_and(|at| same_day(at, now)) && saved.claimed_for_at(&saved.plan, now)
                || saved.owned.as_ref().is_some_and(|owned| {
                    owned.starts_at.is_some_and(|at| same_day(at.saturating_mul(1000), now))
                        && owned.matches_activity(&saved.plan)
                })
            {
                ledger.checkin_targets.entry(plan_id.clone()).or_insert(now);
            }
        }
        let saved = ledger.plans.entry(plan.plan_id.clone()).or_insert_with(|| SavedPlan::new(plan));
        let already_claimed = saved.claimed_for(plan);
        if !already_claimed {
            saved.claimed_at = None;
            saved.claim_starts_at = None;
            saved.claim_ends_at = None;
            saved.owned = None;
        }
        super::plans::merge_plan(&mut saved.plan, plan.clone());
        let outcome = if already_claimed {
            ClaimOutcome::Failed {
                plan_id: plan.plan_id.clone(), failure: ClaimFailure::AlreadyClaimed,
                code: 1003, message: "该套餐已领取，不重复提交".to_string(),
                failure_ends_at: saved.claim_ends_at.or(saved.plan.ends_at),
            }
        } else {
            // 领取只接受当次验证，不读取模型调用的令牌池，也不套用推理免码开关。
            let verification = if captcha.is_none() {
                match claim_verification::prepare(region, account, plan, proxy, deadline).await {
                    Ok(verification) => verification,
                    Err(error) if error.status_code == 428 => {
                        let message = error.message;
                        ledger.checkin_verification = Some(VerificationRequired {
                            at: crate::http::now_ms(), plan_id: plan.plan_id.clone(), message: message.clone(),
                        });
                        if let Err(error) = save(ctx, account, &ledger).await {
                            logging::verbose("[Claim]", &format!("领取验证状态保存失败：{error}"));
                        }
                        let daily_checkin = daily_checkin_view(&ledger, crate::http::now_ms());
                        return Ok(ClaimAttempt {
                            outcome: ClaimOutcome::Failed {
                                plan_id: plan.plan_id.clone(), failure: ClaimFailure::Captcha,
                                code: 428, message, failure_ends_at: None,
                            },
                            marketing_error: None, daily_checkin, claimed_at: None,
                        });
                    }
                    Err(error) => return Err(error.into_message()),
                }
            } else { None };
            let (param, region_tag) = captcha.unwrap_or_else(|| {
                verification.as_ref().map(|value| {
                    (value.param.as_str(), Some(value.region.as_str()))
                }).unwrap_or(("", None))
            });
            let result = claim_upstream(region, jwt, &plan.plan_id, param, region_tag, device_mid, proxy).await;
            ownership::invalidate(region, account, jwt, device_mid, proxy);
            result?
        };
        let settled = matches!(&outcome,
            ClaimOutcome::Claimed { .. }
            | ClaimOutcome::Failed { failure: ClaimFailure::AlreadyClaimed, code: 1003, .. }
        );
        let mut marketing_error = None;
        if let ClaimOutcome::Failed { failure: ClaimFailure::Ineligible, message, .. } = &outcome {
            saved.rejection = Some(Rejection {
                version: app_version(), message: message.clone(), at: crate::http::now_ms(),
            });
            if let Err(error) = save(ctx, account, &ledger).await {
                logging::log("[Claim]", &format!("ZCode 领取资格结果保存失败：{error}"));
            }
        }
        if settled {
            if ledger.checkin_verification.as_ref().is_some_and(|pending| pending.plan_id == plan.plan_id) {
                ledger.checkin_verification = None;
            }
            let saved = ledger.plans.get_mut(&plan.plan_id)
                .ok_or_else(|| GatewayError::with_status(502, "领取台账缺少目标套餐"))?;
            if !already_claimed { saved.claimed_at = Some(crate::http::now_ms()); }
            if saved.claimed_at.is_some_and(|at| same_day(at, crate::http::now_ms())) {
                ledger.checkin_targets.insert(plan.plan_id.clone(), crate::http::now_ms());
            }
            saved.rejection = None;
            match &outcome {
                ClaimOutcome::Claimed { starts_at, ends_at, .. } => {
                    saved.claim_starts_at = *starts_at;
                    saved.claim_ends_at = *ends_at;
                }
                ClaimOutcome::Failed { failure_ends_at, .. } => {
                    saved.claim_ends_at = saved.claim_ends_at.or(*failure_ends_at);
                }
            }
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
        let now = crate::http::now_ms();
        if let ClaimOutcome::Failed { failure: ClaimFailure::Captcha, message, .. } = &outcome {
            ledger.checkin_verification = Some(VerificationRequired {
                at: now, plan_id: plan.plan_id.clone(), message: message.clone(),
            });
            if let Err(error) = save(ctx, account, &ledger).await {
                logging::verbose("[Claim]", &format!("领取验证状态保存失败：{error}"));
            }
        }
        Ok(ClaimAttempt {
            outcome,
            marketing_error,
            daily_checkin: daily_checkin_view(&ledger, now),
            claimed_at: ledger.plans.get(&plan.plan_id).and_then(|saved| saved.claimed_at
                .or_else(|| saved.owned.as_ref().and_then(|owned| owned.starts_at)
                    .map(|at| at.saturating_mul(1000)))),
        })
    }).await.map_err(GatewayError::from)
}

fn bind_identity(
    ledger: &mut Ledger,
    region: Region,
    account: &str,
    jwt: &str,
    device_mid: Option<&str>,
    user_id: &str,
) {
    let subject = if user_id.trim().is_empty() {
        format!("token:{}", crate::flight::fingerprint(jwt.trim()))
    } else {
        format!("user:{}", user_id.trim())
    };
    let identity = format!("v2:{}", crate::flight::fingerprint(&format!(
        "{}:{account}:{subject}", region.provider_id(),
    )));
    if ledger.identity == identity { return; }
    let legacy = crate::flight::fingerprint(&format!("{}:{}", jwt.trim(), device_mid.unwrap_or("")));
    // 旧台账只有凭证指纹；迁移必须能证明仍是原身份，不能把别人的领取记录接过来。
    if ledger.identity != legacy {
        ledger.plans.clear();
        ledger.checkin_targets.clear();
        ledger.checkin_sources_at = None;
        ledger.checkin_sources_complete = false;
    }
    ledger.identity = identity;
}

fn same_day(at: i64, now: i64) -> bool {
    at > 0 && crate::beijing::date_key_of_ms(at) == crate::beijing::date_key_of_ms(now)
}

fn daily_checkin_view(ledger: &Ledger, now: i64) -> Value {
    let current = same_day(ledger.checkin_sources_at.unwrap_or(0), now);
    let targets: Vec<&String> = ledger.checkin_targets.iter()
        .filter(|(_, at)| same_day(**at, now))
        .map(|(id, _)| id)
        .collect();
    let completed_count = targets.iter().filter(|id| {
        ledger.plans.get(**id).is_some_and(|saved| saved.claimed_for_at(&saved.plan, now))
    }).count();
    let completed = current && ledger.checkin_sources_complete && !targets.is_empty()
        && completed_count == targets.len();
    let mut value = serde_json::json!({
        "at": now,
        "kind": "plans",
        "completed": completed,
        "completedCount": completed_count,
        "totalCount": targets.len(),
        "sourcesComplete": current && ledger.checkin_sources_complete,
    });
    if let Some(pending) = ledger.checkin_verification.as_ref().filter(|pending| {
        !completed && same_day(pending.at, now)
            && ledger.plans.get(&pending.plan_id).is_some_and(|saved| !saved.claimed_for_at(&saved.plan, now))
    }) {
        value["status"] = Value::from("captcha");
        value["message"] = Value::from(pending.message.clone());
    }
    value
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
            || saved.owned.as_ref().is_some_and(|owned| owned.plan_id != *id)
    }) {
        return Err(GatewayError::with_status(502, "套餐台账关联异常，未发送领取或回执"));
    }
    Ok(ledger)
}

async fn save(ctx: &PluginCtx, account: &str, ledger: &Ledger) -> Result<(), GatewayError> {
    let value = serde_json::to_value(ledger)
        .map_err(|_| GatewayError::with_status(502, "套餐台账无法序列化"))?;
    ctx.ledger_put(account, LEDGER_KIND, &value).await
        .map_err(|error| GatewayError::with_status(502, format!("保存套餐台账失败：{error}")))?;
    ctx.ledger_put(account, "dailyCheckin", &daily_checkin_view(ledger, crate::http::now_ms())).await
        .map_err(|error| GatewayError::with_status(502, format!("保存每日领取状态失败：{error}")))
}

fn prune(ledger: &mut Ledger, now: i64) {
    ledger.plans.retain(|_, saved| now.saturating_sub(saved.seen_at) <= RETENTION_MS
        || saved.claimed_at.is_some_and(|at| now.saturating_sub(at) <= RETENTION_MS));
    while ledger.plans.len() > MAX_SAVED_PLANS {
        let oldest = ledger.plans.iter().min_by_key(|(_, saved)| saved.seen_at)
            .map(|(id, _)| id.clone());
        match oldest { Some(id) => { ledger.plans.remove(&id); }, None => break }
    }
    ledger.checkin_targets.retain(|id, at| same_day(*at, now) && ledger.plans.contains_key(id));
}
