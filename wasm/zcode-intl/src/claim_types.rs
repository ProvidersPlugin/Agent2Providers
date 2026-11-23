use super::network::{integer, text};
use agent2api_wasm_sdk::{self as sdk, json, Value};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entitlement {
    pub show_name: String,
    pub unit_type: String,
    pub grant_units: Option<i64>,
    pub period: String,
    pub effective_at: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub plan_id: String,
    pub name: String,
    pub description: String,
    pub priority: i64,
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    pub entitlements: Vec<Entitlement>,
    #[serde(default)]
    pub marketing_campaign_ids: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Owned {
    pub plan_id: String,
    pub user_plan_id: Option<String>,
    pub name: String,
    pub state: String,
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    pub pending_until: Option<i64>,
    pub entitlements: Vec<Entitlement>,
}
impl Owned {
    pub fn matches(&self, plan: &Plan) -> bool {
        self.plan_id == plan.plan_id
            && matches!(self.state.as_str(), "active" | "pending")
            && !self.ends_at.is_some_and(|end| end <= sdk::now() / 1000)
            && !plan
                .starts_at
                .zip(self.ends_at)
                .is_some_and(|(start, end)| end <= start)
    }
    pub fn view(&self) -> Value {
        json!({ "planId": self.plan_id, "userPlanId": self.user_plan_id, "name": self.name, "state": self.state,
            "startsAt": self.starts_at, "endsAt": self.ends_at, "pendingUntil": self.pending_until,
            "entitlements": self.entitlements.iter().map(Entitlement::view).collect::<Vec<_>>() })
    }
}
impl Entitlement {
    pub fn view(&self) -> Value {
        json!({ "showName": self.show_name, "unitType": self.unit_type,
        "grantUnits": self.grant_units, "period": self.period, "effectiveAt": self.effective_at })
    }
}
impl Plan {
    pub fn parse(value: &Value) -> Option<Self> {
        let id = identifier(&value["plan_id"])?;
        let name = text(value, &["name"]);
        let entitlements = value["entitlements"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let name = text(item, &["show_name", "entitlement_id"]);
                (!name.is_empty()).then(|| Entitlement {
                    show_name: name,
                    unit_type: text(item, &["unit_type"]),
                    grant_units: integer(&item["grant_units"]),
                    period: text(item, &["period"]),
                    effective_at: integer(&item["effective_at"]),
                })
            })
            .collect();
        Some(Self {
            plan_id: id.to_string(),
            name: if name.is_empty() {
                id.to_string()
            } else {
                name
            },
            description: text(value, &["description"]),
            priority: integer(&value["priority"]).unwrap_or(0),
            starts_at: integer(&value["starts_at"]),
            ends_at: integer(&value["ends_at"]),
            entitlements,
            marketing_campaign_ids: Vec::new(),
        })
    }
    pub fn named(id: &str) -> Self {
        Self {
            plan_id: id.into(),
            name: id.into(),
            description: String::new(),
            priority: 0,
            starts_at: None,
            ends_at: None,
            entitlements: Vec::new(),
            marketing_campaign_ids: Vec::new(),
        }
    }
    pub fn merge(&mut self, incoming: Self) {
        if self.name.is_empty() || self.name == self.plan_id {
            self.name = incoming.name;
        }
        if self.description.is_empty() {
            self.description = incoming.description;
        }
        self.priority = self.priority.max(incoming.priority);
        self.starts_at = self.starts_at.or(incoming.starts_at);
        self.ends_at = self.ends_at.or(incoming.ends_at);
        if self.entitlements.is_empty() {
            self.entitlements = incoming.entitlements;
        }
        for id in incoming.marketing_campaign_ids {
            if !self.marketing_campaign_ids.contains(&id) {
                self.marketing_campaign_ids.push(id);
            }
        }
    }
    pub fn view(&self) -> Value {
        json!({ "planId": self.plan_id, "name": self.name, "description": self.description,
        "priority": self.priority, "startsAt": self.starts_at, "endsAt": self.ends_at,
        "entitlements": self.entitlements.iter().map(Entitlement::view).collect::<Vec<_>>() })
    }
}

pub fn identifier(value: &Value) -> Option<&str> {
    let text = value.as_str()?.trim();
    (!text.is_empty() && text.len() <= 128 && !text.chars().any(char::is_control)).then_some(text)
}
pub fn merge(plans: &mut Vec<Plan>, incoming: Vec<Plan>) {
    for plan in incoming {
        if let Some(previous) = plans.iter_mut().find(|item| item.plan_id == plan.plan_id) {
            previous.merge(plan);
        } else {
            plans.push(plan);
        }
    }
    plans.sort_by(|a, b| b.priority.cmp(&a.priority));
}

#[derive(Default)]
pub struct Discovery {
    pub plans: Vec<Plan>,
    pub account_ids: std::collections::BTreeSet<String>,
    pub marketing_ids: std::collections::BTreeSet<String>,
    pub catalog_ids: std::collections::BTreeSet<String>,
    pub owned: BTreeMap<String, Owned>,
    pub deployed: bool,
    pub activated: Option<bool>,
    pub failures: BTreeMap<String, String>,
    pub daily: Value,
    pub identity: String,
    pub binding: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub claimed: bool,
    pub plan_id: String,
    pub failure: String,
    pub code: i64,
    pub message: String,
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    pub retry_at: Option<i64>,
}
impl Outcome {
    pub fn failed(plan: &Plan, code: i64, message: impl Into<String>, until: Option<i64>) -> Self {
        let failure = match code {
            1001 => "not_found",
            1002 => "unavailable",
            1003 => "already_claimed",
            1004 => "ineligible",
            1005 => "quota_exhausted",
            3001 => "invalid_request",
            3007 | 428 => "captcha",
            401 => "login_required",
            _ => "unknown",
        };
        Self {
            claimed: false,
            plan_id: plan.plan_id.clone(),
            failure: failure.into(),
            code,
            message: message.into(),
            starts_at: None,
            ends_at: until,
            retry_at: None,
        }
    }
    pub fn settled(&self) -> bool {
        self.claimed || self.code == 1003
    }
    pub fn view(&self) -> Value {
        json!({ "ok": self.claimed, "claimed": self.claimed, "planId": self.plan_id,
        "failure": self.failure, "code": self.code, "message": self.message, "startsAt": self.starts_at,
        "endsAt": if self.claimed { self.ends_at } else { None }, "failureEndsAt": if !self.claimed { self.ends_at } else { None },
        "retryAt": self.retry_at })
    }
}
