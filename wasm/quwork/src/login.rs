//! 本机导入不是网页授权：仅显式选择方法0时调用helper，成功读取后才返回可落库引用。
//! 登录状态只保存不含token的引用，宿主分配flow；取消／过期后不能复用。

use super::{account, desktop};
use agent2api_wasm_sdk::{self as sdk, json, state::Scope, Error, Result, Value};

const KEY: &str = "desktop-import";
const TTL: i64 = 120_000;

pub fn start(params: &Value) -> Result<Value> {
    if params["method"]
        .as_str()
        .is_some_and(|method| !method.is_empty() && method != "browser")
        || params["payload"]["methodIndex"].as_u64().unwrap_or(0) != 0
    {
        return Err(Error::http(400, "QuWork 请使用本机导入或粘贴凭证"));
    }
    let flow = params["flow"]
        .as_str()
        .filter(|flow| !flow.is_empty() && flow.len() <= 256)
        .ok_or_else(|| Error::http(400, "主程序未分配 QuWork 导入流程"))?;
    let secret = desktop::reference()?;
    let normalized = account::normalized(secret, params["payload"]["name"].as_str(), true)?;
    let snapshot = sdk::state::get(Scope::Login, KEY, 1)?;
    if !snapshot.compatible
        || !sdk::state::compare_exchange(
            Scope::Login,
            KEY,
            1,
            snapshot.revision,
            json!({ "at": sdk::now(), "account": normalized }),
            TTL,
        )?
    {
        return Err(Error::http(409, "QuWork 导入状态已变化，请重新发起"));
    }
    Ok(json!({ "flow": flow, "url": "", "poll_interval_ms": 1000,
        "instructions": "已读取 QuWork 本机登录态，正在添加只读引用。", "external_browser": false,
        "allowed_hosts": [], "login_ui": { "browserMode": "none", "manualCallback": false,
            "callbackInput": "url", "cookieCapture": null, "captureCallback": false } }))
}

pub fn poll() -> Result<Value> {
    let snapshot = sdk::state::get(Scope::Login, KEY, 1)?;
    let age = sdk::now().saturating_sub(snapshot.value["at"].as_i64().unwrap_or(0));
    if !snapshot.compatible || !(0..TTL).contains(&age) || !snapshot.value["account"].is_object() {
        return Ok(json!({ "state": "failed", "message": "QuWork 导入已失效，请重新发起" }));
    }
    if !sdk::state::compare_exchange(Scope::Login, KEY, 1, snapshot.revision, Value::Null, 1000)? {
        return Err(Error::http(409, "QuWork 导入已处理，请勿重复提交"));
    }
    Ok(json!({ "state": "done", "account": snapshot.value["account"] }))
}

pub fn cancel() -> Result<Value> {
    let snapshot = sdk::state::get(Scope::Login, KEY, 1)?;
    sdk::state::compare_exchange(Scope::Login, KEY, 1, snapshot.revision, Value::Null, 1000)?;
    Ok(Value::Null)
}
