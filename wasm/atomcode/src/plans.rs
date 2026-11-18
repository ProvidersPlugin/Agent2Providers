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
    let now = sdk::now();
    let valid_until = result["until"].as_i64().filter(|until| *until > now);
    let until = if matches!(result["outcome"].as_str(), Some("claimed" | "done")) {
        valid_until.unwrap_or(now + 120_000)
    } else { now + 120_000 };
    if !snapshot.compatible || !sdk::state::compare_exchange(sdk::state::Scope::Account, "claim", 1,
        snapshot.revision, json!({ "until": until,
            "at": now, "result": result }), 0)? {
        return Err(Error::new("套餐结果保存时状态已变化，请刷新套餐状态"));
    }
    Ok(())
}

pub fn status() -> Result<Value> {
    let record = sdk::account()?;
    let token = account::access(&record["secret"])?;
    let status = upstream::status(&token)?;
    let memory = sdk::state::get(sdk::state::Scope::Account, "claim", 1)?;
    if !memory.compatible { return Err(Error::new("套餐领取记忆格式不兼容，请更新插件")); }
    let pending = !usage::active(&status) && memory.value["until"].as_i64().unwrap_or(0) > 0
        && memory.value["result"]["outcome"].is_null();
    let waiting = memory.value["until"].as_i64().is_some_and(|until| until > sdk::now());
    Ok(json!({ "status": match usage::needs_claim(&status) {
        Some(false) => "active", Some(true) => "inactive", None => "unknown"
    }, "claimable": usage::needs_claim(&status) == Some(true) && !pending && !waiting,
        "retryAt": if waiting { memory.value["until"].clone() } else { Value::Null },
        "resultUnconfirmed": pending, "usage": usage::shape(&status) }))
}

pub fn claim() -> Result<Value> {
    account::ensure()?;
    let _lock = AccountLock::acquire()?;
    let record = sdk::account()?;
    let token = account::access(&record["secret"])?;
    let status = upstream::status(&token)?;
    match usage::needs_claim(&status) {
        Some(false) => {
            let result = report("done", "套餐有效中，无需领取", &status);
            let previous = sdk::state::get(sdk::state::Scope::Account, "claim", 1)?;
            if previous.value["until"].as_i64().unwrap_or(0) > 0 && previous.value["result"]["outcome"].is_null() {
                // 未知写入后回读已确认套餐有效，结清原尝试而不再次领取。
                remember(&result)?;
            }
            return Ok(result);
        }
        Some(true) => {},
        None => return Err(Error::new("AtomCode 套餐状态尚未确认，未自动提交领取，请稍后查询")),
    }
    let state = sdk::state::get(sdk::state::Scope::Account, "claim", 1)?;
    if !state.compatible { return Err(Error::new("套餐领取记忆格式不兼容，请先更新插件")); }
    let previous = &state.value;
    if previous["until"].as_i64().unwrap_or(0) > 0 {
        if !previous["result"]["outcome"].is_string() {
            return Err(Error::new("上次套餐领取结果尚未确认，请刷新官方套餐状态；未重复提交领取"));
        }
        if previous["until"].as_i64().is_some_and(|until| until > sdk::now()) {
            let mut result = previous["result"].clone();
            if result["outcome"] == "claimed" { result["outcome"] = json!("done"); }
            return Ok(result);
        }
    }
    if !sdk::state::compare_exchange(sdk::state::Scope::Account, "claim", 1, state.revision,
        json!({ "until": sdk::now() + 120_000, "result": null }), 0)? {
        return Err(Error::new("套餐领取状态已变化，请稍后重试"));
    }
    let mut last_message = String::new();
    for tier in ["Max", "Pro", "Lite"] {
        let response = sdk::http::open_timeout("POST", &format!("{}/coding-plan/claim-v2", upstream::base("codingplanBase")),
            upstream::headers(Some(&token)), Some(&json!({ "plan_type": tier })), 20_000)?;
        let status_code = response.status;
        let bytes = response.bytes(1024 * 1024)?;
        let response: Value = serde_json::from_slice(&bytes)
            .map_err(|_| Error::new("AtomCode 套餐领取结果无法确认，未重复提交"))?;
        let message = account::text(&response, &["message", "msg", "error_message"]);
        if !message.is_empty() { last_message = message; }
        if !response.is_object() || ["success", "duplicate"].iter()
            .any(|key| response.get(*key).is_some_and(|value| !value.is_boolean())) {
            return Err(Error::new("AtomCode 套餐领取结果格式异常，未继续领取"));
        }
        let duplicate = response["duplicate"] == true;
        let upstream_status = super::errors::status(&response).unwrap_or(status_code);
        if matches!(upstream_status, 401 | 403 | 429) || upstream_status >= 500 {
            return Err(super::errors::http_error(upstream_status, &response));
        }
        if !duplicate && !(200..300).contains(&status_code)
            && !([400, 404, 409, 422].contains(&status_code) && response["success"] == false) {
            return Err(super::errors::http_error(status_code, &response));
        }
        if !duplicate && response["success"] != true && response["success"] != false {
            return Err(Error::new("AtomCode 套餐领取响应缺少明确业务结果，已停止后续领取"));
        }
        if duplicate || response["success"] == true {
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
    let message = if last_message.is_empty() {
        "当前没有可领取的免费套餐（活动结束、名额已满或不符合条件）".to_string()
    } else { format!("当前套餐未能领取：{last_message}") };
    let result = report("inactive", &message, &status);
    remember(&result)?;
    Ok(result)
}
