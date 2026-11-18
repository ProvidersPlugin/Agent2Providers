//! 各来源只读元数据，营销资源不加载、不执行；匿名目录不扩大自动领取范围。
use std::collections::BTreeMap;
use agent2api_wasm_sdk::{json, Error, Result, Value};
use super::{account, config, network::{self, integer, text, ZCODE}, claim_types::{self, Plan, Discovery, Owned, Entitlement}, usage_billing};

pub fn billing(record: &Value, anonymous: bool, version: &str) -> Result<Option<Vec<Plan>>> {
    let jwt = if anonymous { String::new() } else { account::jwt(&record["secret"]) };
    let mid = if anonymous { network::uuid()? } else { account::mid(&record["secret"]) };
    let reply = network::request("GET", &config::billing_url("preview", version), config::billing_headers(&jwt, &mid), None)?;
    if reply.status == 404 { return Ok(None); }
    let data = network::envelope(reply, "ZCode 套餐预览")?;
    if !data.is_null() && !data.is_object() || data.get("plans").is_some_and(|plans| !plans.is_null() && !plans.is_array()) {
        return Err(Error::http(502, "ZCode 套餐预览格式无效"));
    }
    Ok(Some(data["plans"].as_array().into_iter().flatten().filter_map(Plan::parse).collect()))
}

fn marketing_headers(record: &Value, version: &str) -> Value {
    let mut headers = config::billing_headers(&account::jwt(&record["secret"]), &account::mid(&record["secret"]));
    headers["X-Client-Language"] = json!("zh-CN");
    headers["X-ZCode-App-Version"] = json!(version);
    headers
}

pub fn marketing(record: &Value, version: &str) -> Result<Vec<Plan>> {
    let reply = network::request("GET", &format!("{ZCODE}/api/v1/marketing/touch?seq=0"), marketing_headers(record, version), None)?;
    if reply.status == 404 { return Ok(Vec::new()); }
    let data = network::envelope(reply, "ZCode 营销套餐查询")?;
    let deliveries = data["deliveries"].as_array().filter(|items| items.len() <= 32)
        .ok_or_else(|| Error::http(502, "ZCode 营销活动列表格式无效"))?;
    let mut plans = Vec::new();
    for delivery in deliveries {
        let Some(campaign) = claim_types::identifier(&delivery["campaign_id"]) else { continue; };
        let mut metadata = BTreeMap::new();
        let mut budget = 4096;
        collect(delivery, 0, &mut budget, &mut metadata);
        let mut found = Vec::new();
        for path in ["/banner/buttons", "/banner/success_popup/buttons", "/popup/buttons"] {
            for button in delivery.pointer(path).and_then(Value::as_array).into_iter().flatten() {
                let action = &button["action"];
                if action["type"] != "claim_zcode_plan" { continue; }
                let Some(id) = claim_types::identifier(&action["args"]["plan_id"]) else { continue; };
                let mut plan = metadata.get(id).cloned().unwrap_or_else(|| Plan::named(id));
                if plan.priority == 0 { plan.priority = integer(&delivery["priority"]).unwrap_or(0); }
                plan.marketing_campaign_ids.push(campaign.to_string());
                found.push(plan);
            }
        }
        claim_types::merge(&mut plans, found);
    }
    Ok(plans)
}

fn collect(value: &Value, depth: usize, remaining: &mut usize, metadata: &mut BTreeMap<String, Plan>) {
    if depth > 12 || *remaining == 0 { return; }
    *remaining -= 1;
    match value {
        Value::Array(items) => for item in items { collect(item, depth + 1, remaining, metadata); if *remaining == 0 { break; } },
        Value::Object(fields) => {
            if let Some(plan) = Plan::parse(value) {
                match metadata.get_mut(&plan.plan_id) {
                    Some(previous) => previous.merge(plan),
                    None => { metadata.insert(plan.plan_id.clone(), plan); }
                }
            }
            for value in fields.values() { collect(value, depth + 1, remaining, metadata); if *remaining == 0 { break; } }
        }
        _ => {}
    }
}

pub fn confirm(record: &Value, version: &str, campaign: &str) -> Result<()> {
    let reply = network::request("POST", &format!("{ZCODE}/api/v1/marketing/touch/action"), marketing_headers(record, version),
        Some(&json!({ "campaign_id": campaign, "action_type": "confirm" })))?;
    network::envelope(reply, "ZCode 营销领取回执").map(|_| ())
}

pub fn ownership(record: &Value) -> Result<Vec<Owned>> {
    let value = usage_billing::fetch(record)?;
    Ok(value["plans"].as_array().into_iter().flatten().filter_map(|plan| {
        let id = claim_types::identifier(&plan["planId"])?;
        Some(Owned { plan_id: id.into(), user_plan_id: plan["userPlanId"].as_str().map(str::to_string),
            name: text(plan, &["name"]), state: text(plan, &["state"]), starts_at: integer(&plan["startsAt"]),
            ends_at: integer(&plan["endsAt"]), pending_until: integer(&plan["pendingUntil"]),
            entitlements: plan["entitlements"].as_array().into_iter().flatten().filter_map(|item| {
                Some(Entitlement { show_name: item["showName"].as_str()?.into(), unit_type: text(item, &["unitType"]),
                    grant_units: integer(&item["grantUnits"]), period: text(item, &["period"]), effective_at: integer(&item["effectiveAt"]) })
            }).collect() })
    }).collect())
}

pub fn account_sources(record: &Value, version: &str, snapshot: &mut Discovery) -> Result<()> {
    match billing(record, false, version) {
        Ok(plans) => {
            snapshot.failures.remove("previewError"); snapshot.deployed |= plans.is_some();
            if let Some(plans) = plans {
                snapshot.account_ids.extend(plans.iter().map(|plan| plan.plan_id.clone()));
                claim_types::merge(&mut snapshot.plans, plans);
            }
        }
        Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
        Err(error) => { snapshot.failures.insert("previewError".into(), error.message); }
    }
    match marketing(record, version) {
        Ok(plans) => {
            snapshot.failures.remove("marketingError"); snapshot.deployed |= !plans.is_empty();
            snapshot.marketing_ids.extend(plans.iter().map(|plan| plan.plan_id.clone()));
            claim_types::merge(&mut snapshot.plans, plans);
        }
        Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
        Err(error) => { snapshot.failures.insert("marketingError".into(), error.message); }
    }
    Ok(())
}
