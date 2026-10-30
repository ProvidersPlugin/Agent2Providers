//! 可领取套餐的统一形态与预览解析。

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ClaimablePlan {
    pub plan_id: String,
    pub name: String,
    pub description: String,
    pub priority: i64,
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    pub entitlements: Vec<PlanEntitlement>,
    #[serde(default)]
    pub marketing_campaign_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlanEntitlement {
    pub show_name: String,
    pub unit_type: String,
    pub grant_units: i64,
    pub period: String,
    pub effective_at: Option<i64>,
}

pub(super) fn parse_plan(raw: &Value) -> Option<ClaimablePlan> {
    let plan_id = identifier(raw.get("plan_id")?)?.to_string();
    let name = text(raw, "name");
    let entitlements = raw
        .get("entitlements")
        .and_then(Value::as_array)
        .map(|items| {
            items.iter().filter_map(|item| {
                let show_name = text(item, "show_name");
                let show_name = if show_name.is_empty() {
                    text(item, "entitlement_id")
                } else {
                    show_name
                };
                if show_name.is_empty() { return None; }
                Some(PlanEntitlement {
                    show_name,
                    unit_type: text(item, "unit_type"),
                    grant_units: item.get("grant_units").and_then(integer).unwrap_or(0),
                    period: text(item, "period"),
                    effective_at: item.get("effective_at").and_then(integer),
                })
            }).collect()
        })
        .unwrap_or_default();
    Some(ClaimablePlan {
        name: if name.is_empty() { plan_id.clone() } else { name },
        plan_id,
        description: text(raw, "description"),
        priority: raw.get("priority").and_then(integer).unwrap_or(0),
        starts_at: raw.get("starts_at").and_then(integer),
        ends_at: raw.get("ends_at").and_then(integer),
        entitlements,
        marketing_campaign_ids: Vec::new(),
    })
}

pub(crate) fn identifier(value: &Value) -> Option<&str> {
    let value = value.as_str()?.trim();
    (!value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control))
        .then_some(value)
}

fn text(raw: &Value, key: &str) -> String {
    raw.get(key).and_then(Value::as_str).map(str::trim).unwrap_or("").to_string()
}

pub(crate) fn integer(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

pub fn pick_target<'a>(plans: &'a [ClaimablePlan], plan_id: &str) -> Option<&'a ClaimablePlan> {
    let wanted = plan_id.trim();
    if !wanted.is_empty() {
        return plans.iter().find(|plan| plan.plan_id == wanted);
    }
    let mut best: Option<&ClaimablePlan> = None;
    for plan in plans {
        if best.is_none_or(|current| plan.priority > current.priority) {
            best = Some(plan);
        }
    }
    best
}

pub(super) fn merge(plans: &mut Vec<ClaimablePlan>, incoming: Vec<ClaimablePlan>) {
    for plan in incoming {
        match plans.iter_mut().find(|item| item.plan_id == plan.plan_id) {
            Some(existing) => merge_plan(existing, plan),
            None => plans.push(plan),
        }
    }
    plans.sort_by(|left, right| right.priority.cmp(&left.priority));
}

pub(super) fn merge_plan(existing: &mut ClaimablePlan, incoming: ClaimablePlan) {
    if existing.name.is_empty() || existing.name == existing.plan_id {
        existing.name = incoming.name;
    }
    if existing.description.is_empty() { existing.description = incoming.description; }
    existing.priority = existing.priority.max(incoming.priority);
    existing.starts_at = existing.starts_at.or(incoming.starts_at);
    existing.ends_at = existing.ends_at.or(incoming.ends_at);
    if existing.entitlements.is_empty() { existing.entitlements = incoming.entitlements; }
    for campaign in incoming.marketing_campaign_ids {
        if !existing.marketing_campaign_ids.contains(&campaign) {
            existing.marketing_campaign_ids.push(campaign);
        }
    }
}
