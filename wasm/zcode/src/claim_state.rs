//! 按账号持久化，沿用原生 zcodeClaimPlans 台账与 v2 身份摘要，避免升级后重复领取。
//! 领取与预览写回都由账号锁保护，短命验证票据不进入此持久台账。
use super::{
    account,
    claim_types::{self, Discovery, Outcome, Owned, Plan},
    network::{self, same_day},
};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const KIND: &str = "zcodeClaimPlans";
const RETENTION: i64 = 90 * 86_400_000;
const PREVIEW_TTL: i64 = 900_000;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default)]
    pub identity: String,
    #[serde(default)]
    pub plans: BTreeMap<String, Saved>,
    #[serde(default)]
    pub checkin_targets: BTreeMap<String, i64>,
    #[serde(default)]
    pub checkin_sources_at: Option<i64>,
    #[serde(default)]
    pub checkin_sources_complete: bool,
    #[serde(default)]
    pub checkin_verification: Option<VerificationRequired>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct VerificationRequired {
    pub at: i64,
    pub plan_id: String,
    pub message: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Saved {
    pub plan: Plan,
    pub seen_at: i64,
    #[serde(default)]
    pub claimed_at: Option<i64>,
    #[serde(default)]
    pub claim_starts_at: Option<i64>,
    #[serde(default)]
    pub claim_ends_at: Option<i64>,
    #[serde(default)]
    pub owned: Option<Owned>,
    #[serde(default)]
    pub rejection: Option<Rejection>,
    #[serde(default)]
    pub confirmed_campaigns: Vec<String>,
    #[serde(default)]
    pub campaign_attempts: BTreeMap<String, i64>,
    #[serde(default)]
    pub retry: Option<Outcome>,
    #[serde(default)]
    pub in_flight_at: Option<i64>,
    #[serde(default)]
    pub in_flight_id: Option<String>,
    #[serde(default)]
    pub submitted_at: Option<i64>,
    #[serde(default)]
    pub submitted_plan: Option<Plan>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Rejection {
    pub version: String,
    pub message: String,
    pub at: i64,
}
impl Saved {
    pub fn new(plan: &Plan) -> Self {
        Self {
            plan: plan.clone(),
            seen_at: sdk::now(),
            claimed_at: None,
            claim_starts_at: None,
            claim_ends_at: None,
            owned: None,
            rejection: None,
            confirmed_campaigns: Vec::new(),
            campaign_attempts: BTreeMap::new(),
            retry: None,
            in_flight_at: None,
            in_flight_id: None,
            submitted_at: None,
            submitted_plan: None,
        }
    }
    pub fn claimed_for(&self, plan: &Plan) -> bool {
        let Some(at) = self.claimed_at else {
            return self.owned.as_ref().is_some_and(|owned| owned.matches(plan));
        };
        let end = self.claim_ends_at.or(self.plan.ends_at);
        if end.is_some_and(|end| end <= sdk::now() / 1000) {
            return false;
        }
        match (plan.starts_at, end) {
            (Some(start), Some(end)) => end > start,
            _ => !plan
                .starts_at
                .is_some_and(|start| start > self.claim_starts_at.unwrap_or(at / 1000)),
        }
    }
    pub fn claimed_at(&self) -> Option<i64> {
        self.claimed_at.or_else(|| {
            self.owned
                .as_ref()
                .and_then(|owned| owned.starts_at)
                .map(|at| at.saturating_mul(1000))
        })
    }
}

impl Ledger {
    pub fn load(record: &Value) -> Result<Self> {
        let value = sdk::host("ledger.get", json!({ "kind": KIND }))?;
        if !value.is_null() && !value.is_object() {
            return Err(Error::new("ZCode 套餐台账不是对象"));
        }
        let mut ledger: Self = if value.is_null() {
            Self::default()
        } else {
            serde_json::from_value(value)
                .map_err(|_| Error::new("ZCode 套餐台账格式无效，未执行领取"))?
        };
        for (id, saved) in &ledger.plans {
            if claim_types::identifier(&json!(id)).is_none()
                || saved.plan.plan_id != *id
                || saved
                    .plan
                    .marketing_campaign_ids
                    .iter()
                    .any(|id| claim_types::identifier(&json!(id)).is_none())
                || saved
                    .confirmed_campaigns
                    .iter()
                    .any(|id| claim_types::identifier(&json!(id)).is_none())
                || saved
                    .campaign_attempts
                    .keys()
                    .any(|id| claim_types::identifier(&json!(id)).is_none())
                || saved
                    .submitted_plan
                    .as_ref()
                    .is_some_and(|plan| plan.plan_id != *id)
                || saved
                    .owned
                    .as_ref()
                    .is_some_and(|owned| owned.plan_id != *id)
            {
                return Err(Error::new("ZCode 套餐台账关联异常，未执行领取或回执"));
            }
        }
        let jwt = account::jwt(&record["secret"]);
        let user = account::user(&record["secret"]);
        let subject = if user.is_empty() {
            format!("token:{}", network::hash(jwt.trim()))
        } else {
            format!("user:{}", user.trim())
        };
        let identity = format!(
            "v2:{}",
            network::hash(&format!(
                "zcode:{}:{subject}",
                record["id"].as_str().unwrap_or("")
            ))
        );
        if ledger.identity != identity {
            let legacy = network::hash(&format!(
                "{}:{}",
                jwt.trim(),
                account::mid(&record["secret"])
            ));
            if ledger.identity != legacy {
                ledger = Self::default();
            }
            ledger.identity = identity;
        }
        ledger.prune();
        Ok(ledger)
    }
    pub fn save(&mut self) -> Result<()> {
        self.prune();
        let value =
            serde_json::to_value(&*self).map_err(|_| Error::new("ZCode 套餐台账不能序列化"))?;
        if value.to_string().len() > 60 * 1024 {
            return Err(Error::new("ZCode 套餐台账超过存储限制，未执行领取"));
        }
        sdk::host("ledger.put", json!({ "kind": KIND, "value": value }))?;
        sdk::host(
            "ledger.put",
            json!({ "kind": "dailyCheckin", "value": self.daily() }),
        )?;
        Ok(())
    }
    fn prune(&mut self) {
        let now = sdk::now();
        self.plans.retain(|_, saved| {
            now.saturating_sub(saved.seen_at) <= RETENTION
                || saved
                    .claimed_at
                    .is_some_and(|at| now.saturating_sub(at) <= RETENTION)
        });
        while self.plans.len() > 256 {
            let oldest = self
                .plans
                .iter()
                .min_by_key(|(_, saved)| saved.seen_at)
                .map(|(id, _)| id.clone());
            if let Some(id) = oldest {
                self.plans.remove(&id);
            } else {
                break;
            }
        }
        self.checkin_targets
            .retain(|id, at| same_day(*at, now) && self.plans.contains_key(id));
    }
    pub fn known(&self, id: &str) -> Option<Plan> {
        self.plans
            .get(id)
            .filter(|saved| {
                saved.claimed_for(&saved.plan)
                    || sdk::now().saturating_sub(saved.seen_at) <= PREVIEW_TTL
            })
            .map(|saved| saved.plan.clone())
    }
    pub fn retained(&self, display: bool) -> Vec<Plan> {
        self.plans
            .iter()
            .filter(|(id, saved)| {
                self.checkin_targets
                    .get(*id)
                    .is_some_and(|at| same_day(*at, sdk::now()))
                    || saved.claimed_at.is_some_and(|at| same_day(at, sdk::now()))
                        && saved.claimed_for(&saved.plan)
                    || display
                        && saved.claimed_for(&saved.plan)
                        && (saved
                            .claim_ends_at
                            .or(saved.plan.ends_at)
                            .is_some_and(|end| end > sdk::now() / 1000)
                            || saved.owned.is_some()
                            || saved
                                .claimed_at
                                .is_some_and(|at| sdk::now().saturating_sub(at) <= 86_400_000))
            })
            .map(|(_, saved)| saved.plan.clone())
            .collect()
    }
    pub fn remember(&mut self, snapshot: &Discovery) -> Result<()> {
        let now = sdk::now();
        let today = self.checkin_sources_at.is_some_and(|at| same_day(at, now));
        if !today {
            self.checkin_targets.clear();
        }
        let source_failures = snapshot.failures.keys().any(|key| key != "activationError");
        self.checkin_sources_complete = !source_failures;
        self.checkin_sources_at = Some(now);
        for id in snapshot.account_ids.union(&snapshot.marketing_ids) {
            self.checkin_targets.insert(id.clone(), now);
        }
        for plan in &snapshot.plans {
            let saved = self
                .plans
                .entry(plan.plan_id.clone())
                .or_insert_with(|| Saved::new(plan));
            let mut fresh = plan.clone();
            fresh.merge(saved.plan.clone());
            if !saved.claimed_for(&fresh) {
                saved.claimed_at = None;
                saved.claim_starts_at = None;
                saved.claim_ends_at = None;
                saved.owned = None;
            }
            saved.plan = fresh;
            saved.seen_at = now;
            if let Some(owned) = snapshot
                .owned
                .get(&plan.plan_id)
                .filter(|owned| owned.matches(plan))
            {
                saved.owned = Some(owned.clone());
                saved.rejection = None;
                saved.retry = None;
                saved.submitted_at = None;
                saved.submitted_plan = None;
            }
        }
        self.add_recent_targets();
        self.save()
    }
    pub fn add_recent_targets(&mut self) {
        for (id, saved) in &self.plans {
            if saved.claimed_at.is_some_and(|at| same_day(at, sdk::now()))
                && saved.claimed_for(&saved.plan)
                || saved.owned.as_ref().is_some_and(|owned| {
                    owned
                        .starts_at
                        .is_some_and(|at| same_day(at.saturating_mul(1000), sdk::now()))
                        && owned.matches(&saved.plan)
                })
            {
                self.checkin_targets.insert(id.clone(), sdk::now());
            }
        }
    }
    pub fn daily(&self) -> Value {
        let now = sdk::now();
        let current = self.checkin_sources_at.is_some_and(|at| same_day(at, now));
        let targets = self
            .checkin_targets
            .iter()
            .filter(|(_, at)| same_day(**at, now))
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        let count = targets
            .iter()
            .filter(|id| {
                self.plans
                    .get(**id)
                    .is_some_and(|saved| saved.claimed_for(&saved.plan))
            })
            .count();
        let complete = current
            && self.checkin_sources_complete
            && !targets.is_empty()
            && count == targets.len();
        let mut view = json!({ "at": now, "kind": "plans", "completed": complete,
            "completedCount": count, "totalCount": targets.len(), "sourcesComplete": current && self.checkin_sources_complete });
        if let Some(pending) = &self.checkin_verification {
            if !complete
                && same_day(pending.at, now)
                && self
                    .plans
                    .get(&pending.plan_id)
                    .is_some_and(|saved| !saved.claimed_for(&saved.plan))
            {
                view["status"] = json!("captcha");
                view["message"] = json!(pending.message);
            }
        }
        view
    }
}
