use super::{
    account, activation, claim_attempt, claim_sources,
    claim_state::Ledger,
    claim_types::{self, Discovery, Plan},
    config, network,
};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

pub fn discover(display: bool, activate_empty: bool) -> Result<Discovery> {
    let record = account::with_device()?;
    if account::jwt(&record["secret"]).is_empty() {
        return Err(Error::http(400, "ZCode 账号缺少套餐 JWT，无法查询赠送套餐"));
    }
    let version = config::app_version()?;
    let mut snapshot = Discovery::default();
    claim_sources::account_sources(&record, &version, &mut snapshot)?;
    if activate_empty
        && snapshot.plans.is_empty()
        && snapshot.deployed
        && !account::user(&record["secret"]).is_empty()
    {
        let activation = activation::report(&record)?;
        snapshot.activated = Some(activation.is_none());
        if let Some(error) = activation {
            snapshot.failures.insert("activationError".into(), error);
        }
        claim_sources::account_sources(&record, &version, &mut snapshot)?;
    }
    let ledger = Ledger::load(&record)?;
    snapshot.identity = ledger.identity.clone();
    snapshot.binding = config::model_verification_binding(&record);
    claim_types::merge(&mut snapshot.plans, ledger.retained(display));
    if display {
        match claim_sources::billing(&record, true, &version) {
            Ok(Some(plans)) => {
                snapshot
                    .catalog_ids
                    .extend(plans.iter().map(|plan| plan.plan_id.clone()));
                snapshot.deployed |= !plans.is_empty();
                claim_types::merge(&mut snapshot.plans, plans);
            }
            Ok(None) => {}
            Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
            Err(error) => {
                snapshot
                    .failures
                    .insert("catalogError".into(), error.message);
            }
        }
    }
    match claim_sources::ownership(&record) {
        Ok(owned) => {
            for plan in &snapshot.plans {
                if let Some(owned) = owned
                    .iter()
                    .filter(|owned| owned.matches(plan))
                    .max_by_key(|owned| owned.starts_at)
                {
                    snapshot.owned.insert(plan.plan_id.clone(), owned.clone());
                }
            }
        }
        Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
        Err(error) => {
            snapshot
                .failures
                .insert("ownershipError".into(), error.message);
        }
    }
    {
        let _lock = AccountLock::acquire()?;
        let current = account::record()?;
        if current["secret"] != record["secret"] {
            return Err(Error::http(409, "ZCode 账号已变化，未保存旧账号的套餐状态"));
        }
        let mut ledger = Ledger::load(&current)?;
        ledger.remember(&snapshot)?;
        snapshot.daily = ledger.daily();
    }
    if snapshot.plans.is_empty()
        && snapshot.failures.contains_key("previewError")
        && snapshot.failures.contains_key("marketingError")
        && (!display || snapshot.failures.contains_key("catalogError"))
    {
        return Err(Error::http(
            502,
            "ZCode 套餐来源均未能查询，不能把失败视为无活动",
        ));
    }
    Ok(snapshot)
}

pub fn preview(display: bool, activate_empty: bool) -> Result<Value> {
    let snapshot = discover(display, activate_empty)?;
    let record = account::record()?;
    let ledger = Ledger::load(&record)?;
    if ledger.identity != snapshot.identity
        || config::model_verification_binding(&record) != snapshot.binding
    {
        return Err(Error::http(
            409,
            "ZCode 账号身份或凭据已变化，请重新查询套餐",
        ));
    }
    let version = config::app_version()?;
    let plans = snapshot
        .plans
        .iter()
        .map(|plan| {
            let saved = ledger.plans.get(&plan.plan_id);
            let claimed = saved.is_some_and(|saved| saved.claimed_for(plan));
            let rejection = saved
                .and_then(|saved| saved.rejection.as_ref())
                .filter(|rejection| {
                    rejection.version == version
                        && sdk::now().saturating_sub(rejection.at) <= 900_000
                });
            let mut value = plan.view();
            let mut sources = Vec::new();
            if snapshot.account_ids.contains(&plan.plan_id) {
                sources.push("preview");
            }
            if snapshot.marketing_ids.contains(&plan.plan_id) {
                sources.push("marketing");
            }
            if snapshot.catalog_ids.contains(&plan.plan_id) {
                sources.push("catalog");
            }
            if sources.is_empty() {
                sources.push("ledger");
            }
            value["claimed"] = json!(claimed);
            value["claimedAt"] = json!(saved.and_then(|saved| saved.claimed_at()));
            value["claimState"] = json!(if claimed {
                "claimed"
            } else if rejection.is_some() {
                "ineligible"
            } else if snapshot.account_ids.contains(&plan.plan_id) {
                "claimable"
            } else {
                "unknown"
            });
            value["claimMessage"] = json!(rejection.map(|rejection| &rejection.message));
            value["source"] = json!(sources[0]);
            value["sources"] = json!(sources);
            value["ownedPlan"] = saved
                .and_then(|saved| saved.owned.as_ref())
                .filter(|owned| owned.matches(plan))
                .map(|owned| owned.view())
                .unwrap_or(Value::Null);
            if saved.is_some_and(|saved| !saved.claimed_for(plan) && saved.in_flight_id.is_some()) {
                value["claimState"] = json!("unknown");
                value["claimMessage"] = json!("前次领取结果尚未确认，请查询持有套餐；不会重复提交");
            }
            if let Some(saved) = saved {
                value["claimStartsAt"] = json!(saved.claim_starts_at);
                value["claimEndsAt"] = json!(saved.claim_ends_at);
            }
            value
        })
        .collect::<Vec<_>>();
    let mut result = json!({ "plans": plans, "deployed": snapshot.deployed, "activated": snapshot.activated,
        "activationError": null, "previewError": null, "marketingError": null, "catalogError": null,
        "ownershipError": null, "ledgerError": null, "dailyCheckin": snapshot.daily });
    for (field, message) in snapshot.failures {
        result[field] = json!(message);
    }
    Ok(result)
}

pub fn resolve(requested: &str) -> Result<Option<Plan>> {
    let record = account::with_device()?;
    let ledger = Ledger::load(&record)?;
    if !requested.is_empty() && claim_types::identifier(&json!(requested)).is_none() {
        return Err(Error::http(400, "套餐标识格式无效"));
    }
    let snapshot = discover(false, false);
    if account::record()?["secret"] != record["secret"] {
        return Err(Error::http(409, "ZCode 账号已变化，未复用旧账号套餐目录"));
    }
    if !requested.is_empty() {
        let snapshot = snapshot?;
        let plan = snapshot
            .plans
            .into_iter()
            .find(|plan| plan.plan_id == requested)
            .or_else(|| ledger.known(requested));
        return Ok(Some(plan.unwrap_or_else(|| Plan::named(requested))));
    }
    let snapshot = snapshot?;
    let ledger = Ledger::load(&account::record()?)?;
    Ok(snapshot.plans.into_iter().find(|plan| {
        !ledger
            .plans
            .get(&plan.plan_id)
            .is_some_and(|saved| saved.claimed_for(plan))
    }))
}

pub fn manual(payload: &Value, supplied_verification: bool) -> Result<Value> {
    let record = account::with_device()?;
    if supplied_verification {
        super::verification::check_supplied(payload, &record)?;
    }
    if account::jwt(&record["secret"]).is_empty() {
        return Err(Error::http(400, "ZCode 账号没有套餐 JWT，无法领取"));
    }
    let activation = activation::report(&record)?;
    let requested = network::text(payload, &["planId"]);
    let Some(plan) = resolve(&requested)? else {
        return Ok(
            json!({ "ok": false, "claimed": false, "failure": "unavailable", "message": "当前没有可领取的套餐" }),
        );
    };
    let proof = supplied_verification.then_some(payload);
    let attempt = claim_attempt::execute(&record, &plan, proof, sdk::now() + 100_000)?;
    let mut result = attempt.view;
    result["activated"] =
        json!(!account::user(&record["secret"]).is_empty() && activation.is_none());
    result["activationError"] = json!(activation);
    Ok(result)
}
