use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

use super::{account, models, upstream, usage};

fn report(outcome: &str, message: &str, status: &Value) -> Value {
    let mut value = json!({ "outcome": outcome, "message": message });
    if let Some(until) = usage::active_until(status) { value["until"] = json!(until); }
    value["details"] = usage::shape(status);
    value
}

fn remember(result: &Value) -> Result<()> {
    let snapshot = sdk::state::get(sdk::state::Scope::Account, "claim", 1)?;
    if !snapshot.compatible || !sdk::state::compare_exchange(sdk::state::Scope::Account, "claim", 1,
        snapshot.revision, json!({ "until": sdk::now() + 120_000, "result": result }), 0)? {
        return Err(Error::new("套餐结果保存时状态已变化，请刷新套餐状态"));
    }
    Ok(())
}

pub fn claim() -> Result<Value> {
    account::ensure()?;
    let _lock = AccountLock::acquire()?;
    let record = sdk::account()?;
    let token = account::text(&record["secret"], &["accessToken", "access_token", "token"]);
    let status = upstream::status(&token)?;
    if usage::active(&status) { return Ok(report("done", "套餐有效中，无需领取", &status)); }
    let state = sdk::state::get(sdk::state::Scope::Account, "claim", 1)?;
    if !state.compatible { return Err(Error::new("套餐领取记忆格式不兼容，请先更新插件")); }
    let previous = &state.value;
    if previous["until"].as_i64().is_some_and(|until| until > sdk::now()) {
        return if previous["result"]["outcome"].is_string() { Ok(previous["result"].clone()) }
        else { Err(Error::new("上次套餐领取结果尚未确认，请稍后刷新套餐状态，不重复领取")) };
    }
    if !sdk::state::compare_exchange(sdk::state::Scope::Account, "claim", 1, state.revision,
        json!({ "until": sdk::now() + 120_000, "result": null }), 0)? {
        return Err(Error::new("套餐领取状态已变化，请稍后重试"));
    }
    for tier in ["Max", "Pro", "Lite"] {
        let response = upstream::post("/coding-plan/claim-v2", &token, &json!({ "plan_type": tier }))?;
        if !response["success"].is_boolean() && !response["duplicate"].is_boolean() {
            return Err(Error::new("AtomCode 套餐领取响应缺少业务结果，已停止后续领取"));
        }
        if response["duplicate"] == true || response["success"] == true {
            let duplicate = response["duplicate"] == true;
            let refreshed_status = upstream::status(&token).unwrap_or(Value::Null);
            let mut result = report(if duplicate { "done" } else { "claimed" },
                if duplicate { "已持有套餐，无需重复领取" } else { "免费 CodingPlan 领取成功" }, &refreshed_status);
            result["credit"] = json!(format!("{tier} 档免费套餐"));
            match models::fetch(&token) {
                Ok(catalog) => { result["catalog"] = catalog; }
                Err(_) => { result["catalogRefreshRequired"] = json!(true); }
            }
            remember(&result)?;
            return Ok(result);
        }
    }
    let result = report("inactive", "当前没有可领取的免费套餐（活动结束、名额已满或不符合条件）", &status);
    remember(&result)?;
    Ok(result)
}
