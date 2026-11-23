use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope, Snapshot},
    Error, Result, Value,
};

use crate::util;

const KEY: &str = "codar-login";
pub const TTL: i64 = 600_000;

pub fn flow(params: &Value) -> Result<&str> {
    params["flow"]
        .as_str()
        .filter(|flow| {
            flow.starts_with("login_") && flow.len() <= 256 && !flow.chars().any(char::is_control)
        })
        .ok_or_else(|| {
            Error::http(400, "主程序未提供可信科搭登录流程").with_code("login_flow_required")
        })
}

pub fn load() -> Result<Snapshot> {
    let state = state::get(Scope::Login, KEY, 1)?;
    if !state.compatible {
        return Err(Error::http(409, "科搭登录状态格式已变化，请重新发起"));
    }
    Ok(state)
}

pub fn save(old: &Snapshot, value: Value) -> Result<()> {
    if old.value["deadline"]
        .as_i64()
        .is_some_and(|deadline| deadline <= sdk::now())
    {
        return Err(Error::http(409, "科搭登录已过期，未保存迟到结果"));
    }
    let ttl = value["deadline"]
        .as_i64()
        .map(|deadline| deadline.saturating_sub(sdk::now()).clamp(1000, TTL))
        .unwrap_or(TTL);
    if !state::compare_exchange(Scope::Login, KEY, 1, old.revision, value, ttl)? {
        return Err(Error::http(409, "科搭登录步骤已变化，未覆盖其他操作"));
    }
    Ok(())
}

pub fn live(old: &Snapshot) -> Result<()> {
    if !old.value.is_object()
        || old.value["deadline"]
            .as_i64()
            .is_none_or(|deadline| deadline <= sdk::now())
        || matches!(
            old.value["phase"].as_str(),
            Some("done" | "cancelled" | "failed")
        )
    {
        return Err(Error::http(409, "科搭登录已结束或过期，请重新发起"));
    }
    Ok(())
}

pub fn unchanged(old: &Snapshot) -> Result<()> {
    let current = load()?;
    live(&current)?;
    if current.revision != old.revision {
        return Err(Error::http(409, "科搭登录已取消或步骤已变化"));
    }
    Ok(())
}

pub fn reserve(old: &Snapshot, phase: &str) -> Result<Snapshot> {
    live(old)?;
    let mut value = old.value.clone();
    let marker = util::nonce()?;
    value["phase"] = json!(phase);
    value["marker"] = json!(marker);
    value["inFlightUntil"] = json!(sdk::now()
        .saturating_add(60_000)
        .min(value["deadline"].as_i64().unwrap_or(0)));
    save(old, value)?;
    let current = load()?;
    if current.value["marker"] != marker || current.value["phase"] != phase {
        return Err(Error::http(409, "科搭登录已取消或被其他步骤更新"));
    }
    Ok(current)
}

pub fn cancel() -> Result<Value> {
    for _ in 0..3 {
        let old = load()?;
        if state::compare_exchange(
            Scope::Login,
            KEY,
            1,
            old.revision,
            json!({ "phase": "cancelled" }),
            1000,
        )? {
            return Ok(Value::Null);
        }
    }
    Err(Error::http(409, "科搭登录正被更新，请稍后取消"))
}
