//! 可信根flow限定的登录状态；无全局缓存/后台线程，取消写墓碑，CAS阻止迟到回包复活流程。

use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{Scope, Snapshot},
    Error, Result, Value,
};
use base64::{engine::general_purpose::STANDARD, Engine};

const KEY: &str = "astudio-login";
pub const TTL: i64 = 600_000;

pub fn flow(params: &Value, sms: bool) -> Result<&str> {
    params["flow"]
        .as_str()
        .filter(|value| {
            value.len() <= 256
                && !value.chars().any(char::is_control)
                && value.starts_with(if sms { "sms_" } else { "login_" })
        })
        .ok_or_else(|| {
            Error::http(400, "主程序未提供可信 AStudio 登录流程").with_code("login_flow_required")
        })
}

pub fn load() -> Result<Snapshot> {
    let state = sdk::state::get(Scope::Login, KEY, 1)?;
    if !state.compatible {
        return Err(Error::http(409, "AStudio 登录状态版本已变化，请重新发起"));
    }
    Ok(state)
}

pub fn save(previous: &Snapshot, value: Value) -> Result<()> {
    if previous.value["deadline"]
        .as_i64()
        .is_some_and(|at| at <= sdk::now())
    {
        let _ = sdk::state::compare_exchange(
            Scope::Login,
            KEY,
            1,
            previous.revision,
            json!({ "phase": "cancelled" }),
            TTL,
        );
        return Err(Error::http(409, "AStudio 登录已过期，未保存迟到结果"));
    }
    if !sdk::state::compare_exchange(Scope::Login, KEY, 1, previous.revision, value, TTL)? {
        return Err(Error::http(409, "AStudio 登录状态已变化，未覆盖新的步骤"));
    }
    Ok(())
}

pub fn nonce() -> Result<String> {
    let value = sdk::host("random.bytes", json!({ "length": 24 }))?;
    let bytes = STANDARD
        .decode(value.as_str().unwrap_or(""))
        .map_err(|_| Error::http(502, "主程序随机数返回格式异常"))?;
    if bytes.len() != 24 {
        return Err(Error::http(502, "主程序随机数长度异常"));
    }
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn reserve(previous: &Snapshot, mut value: Value, phase: &str) -> Result<Snapshot> {
    let marker = nonce()?;
    value["phase"] = json!(phase);
    value["marker"] = json!(marker);
    save(previous, value)?;
    let reserved = load()?;
    if reserved.value["marker"] != marker || reserved.value["phase"] != phase {
        return Err(Error::http(409, "AStudio 登录已取消或正在处理其他步骤"));
    }
    Ok(reserved)
}

pub fn live(state: &Snapshot) -> Result<()> {
    if state.value["phase"] == "cancelled"
        || state.value["phase"] == "done"
        || state.value["deadline"]
            .as_i64()
            .is_none_or(|at| at <= sdk::now())
    {
        return Err(Error::http(409, "AStudio 登录已结束或过期，请重新发起"));
    }
    Ok(())
}

pub fn ensure_current(previous: &Snapshot) -> Result<()> {
    let current = load()?;
    live(&current)?;
    if current.revision != previous.revision {
        return Err(Error::http(409, "AStudio 登录已取消或步骤已变化"));
    }
    Ok(())
}

pub fn cancel() -> Result<Value> {
    for _ in 0..3 {
        let previous = load()?;
        if sdk::state::compare_exchange(
            Scope::Login,
            KEY,
            1,
            previous.revision,
            json!({ "phase": "cancelled" }),
            TTL,
        )? {
            return Ok(Value::Null);
        }
    }
    Err(Error::http(409, "AStudio 登录状态仍在变化，请稍后取消"))
}
