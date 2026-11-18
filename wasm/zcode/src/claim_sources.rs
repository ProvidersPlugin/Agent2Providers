//! 各来源只读元数据，营销资源不加载、不执行；匿名目录不扩大自动领取范围。
use super::{
    account,
    claim_types::{self, Discovery, Entitlement, Owned, Plan},
    config,
    network::{self, integer, text, ZCODE},
    usage_billing,
};
use agent2api_wasm_sdk::{json, Error, Result, Value};
use std::collections::BTreeMap;

pub fn billing(record: &Value, anonymous: bool, version: &str) -> Result<Option<Vec<Plan>>> {
    let jwt = if anonymous {
        String::new()
    } else {
        account::jwt(&record["secret"])
    };
    let mid = if anonymous {
        network::uuid()?
    } else {
        account::mid(&record["secret"])
    };
    let reply = network::request(
        "GET",
        &config::billing_url("preview", version),
        config::billing_headers(&jwt, &mid),
        None,
    )?;
    if reply.status == 404 {
        return Ok(None);
    }
    let data = network::envelope(reply, "ZCode 套餐预览")?;
    if !data.is_null() && !data.is_object()
        || data
            .get("plans")
            .is_some_and(|plans| !plans.is_null() && !plans.is_array())
    {
        return Err(Error::http(502, "ZCode 套餐预览格式无效"));
    }
    Ok(Some(
        data["plans"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Plan::parse)
            .collect(),
    ))
}

fn marketing_headers(record: &Value, version: &str) -> Value {
    let mut headers = config::billing_headers(
        &account::jwt(&record["secret"]),
        &account::mid(&record["secret"]),
    );
    headers["X-Client-Language"] = json!("zh-CN");
    headers["X-ZCode-App-Version"] = json!(version);
    headers["X-Platform"] = json!(config::PLATFORM);
    headers
}

pub fn marketing(record: &Value, version: &str) -> Result<Option<Vec<Plan>>> {
    let reply = network::request(
        "GET",
        &format!("{ZCODE}/api/v1/marketing/touch?seq=0"),
        marketing_headers(record, version),
        None,
    )?;
    if reply.status == 404 {
        return Ok(None);
    }
    let data = network::envelope(reply, "ZCode 营销套餐查询")?;
    if data.is_null() {
        return Ok(Some(Vec::new()));
    }
    if !data.is_object() {
        return Err(Error::http(502, "ZCode 营销活动响应格式无效"));
    }
    if data["deliveries"].is_null() {
        return Ok(Some(Vec::new()));
    }
    let deliveries = data["deliveries"]
        .as_array()
        .filter(|items| items.len() <= 32)
        .ok_or_else(|| Error::http(502, "ZCode 营销活动列表格式无效"));
    let deliveries = deliveries?;
    let mut plans = Vec::new();
    for delivery in deliveries {
        let campaign = claim_types::identifier(&delivery["campaign_id"]);
        let mut metadata = BTreeMap::new();
        let mut actions = Vec::new();
        let mut budget = 4096;
        collect(delivery, 0, &mut budget, &mut metadata, &mut actions);
        let mut found = Vec::new();
        for id in actions {
            let mut plan = metadata
                .get(&id)
                .cloned()
                .unwrap_or_else(|| Plan::named(&id));
            if plan.priority == 0 {
                plan.priority = integer(&delivery["priority"]).unwrap_or(0);
            }
            if let Some(campaign) = campaign {
                plan.marketing_campaign_ids.push(campaign.to_string());
            }
            found.push(plan);
        }
        claim_types::merge(&mut plans, found);
    }
    Ok(Some(plans))
}

fn collect(
    value: &Value,
    depth: usize,
    remaining: &mut usize,
    metadata: &mut BTreeMap<String, Plan>,
    actions: &mut Vec<String>,
) {
    if depth > 12 || *remaining == 0 {
        return;
    }
    *remaining -= 1;
    match value {
        Value::Array(items) => {
            for item in items {
                collect(item, depth + 1, remaining, metadata, actions);
                if *remaining == 0 {
                    break;
                }
            }
        }
        Value::Object(fields) => {
            if value["type"] == "claim_zcode_plan" {
                if let Some(id) = claim_types::identifier(&value["args"]["plan_id"]) {
                    if !actions.iter().any(|known| known == id) {
                        actions.push(id.to_string());
                    }
                }
            }
            if let Some(plan) = Plan::parse(value) {
                match metadata.get_mut(&plan.plan_id) {
                    Some(previous) => previous.merge(plan),
                    None => {
                        metadata.insert(plan.plan_id.clone(), plan);
                    }
                }
            }
            for value in fields.values() {
                collect(value, depth + 1, remaining, metadata, actions);
                if *remaining == 0 {
                    break;
                }
            }
        }
        _ => {}
    }
}

pub fn confirm(record: &Value, version: &str, campaign: &str) -> Result<()> {
    let reply = network::request(
        "POST",
        &format!("{ZCODE}/api/v1/marketing/touch/action"),
        marketing_headers(record, version),
        Some(&json!({ "campaign_id": campaign, "action_type": "confirm" })),
    )?;
    network::envelope(reply, "ZCode 营销领取回执").map(|_| ())
}

pub fn ownership(record: &Value) -> Result<Vec<Owned>> {
    let value = usage_billing::fetch(record)?;
    Ok(value["plans"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|plan| {
            let id = claim_types::identifier(&plan["planId"])?;
            Some(Owned {
                plan_id: id.into(),
                user_plan_id: plan["userPlanId"].as_str().map(str::to_string),
                name: text(plan, &["name"]),
                state: text(plan, &["state"]),
                starts_at: integer(&plan["startsAt"]),
                ends_at: integer(&plan["endsAt"]),
                pending_until: integer(&plan["pendingUntil"]),
                entitlements: plan["entitlements"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|item| {
                        Some(Entitlement {
                            show_name: item["showName"].as_str()?.into(),
                            unit_type: text(item, &["unitType"]),
                            grant_units: integer(&item["grantUnits"]).unwrap_or(0),
                            period: text(item, &["period"]),
                            effective_at: integer(&item["effectiveAt"]),
                        })
                    })
                    .collect(),
            })
        })
        .collect())
}

pub fn account_sources(record: &Value, version: &str, snapshot: &mut Discovery) -> Result<()> {
    // v3.17 起官方领取入口只由营销 feed 决定；旧 preview 只在接口明确不存在时兼容回落。
    match marketing(record, version) {
        Ok(Some(plans)) => {
            snapshot.deployed = true;
            snapshot
                .marketing_ids
                .extend(plans.iter().map(|plan| plan.plan_id.clone()));
            claim_types::merge(&mut snapshot.plans, plans);
            return Ok(());
        }
        Ok(None) => {}
        Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
        Err(error) => {
            snapshot
                .failures
                .insert("marketingError".into(), error.message);
            return Ok(());
        }
    }
    match billing(record, false, version) {
        Ok(Some(plans)) => {
            snapshot.deployed = true;
            snapshot
                .account_ids
                .extend(plans.iter().map(|plan| plan.plan_id.clone()));
            claim_types::merge(&mut snapshot.plans, plans);
        }
        Ok(None) => {}
        Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
        Err(error) => {
            snapshot
                .failures
                .insert("previewError".into(), error.message);
        }
    }
    Ok(())
}
