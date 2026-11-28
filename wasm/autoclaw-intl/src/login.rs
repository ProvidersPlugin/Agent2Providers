//! 国际版网页登录使用独立宿主 flow；本机方法4由添加账号的显式导入入口处理。
//! 保留旧带用户授权的 method4 流程兼容，但不把它当作 Zai OAuth。
use super::{account, desktop, oauth::Vendor, util, web_login};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    Error, Result, Value,
};

const DESKTOP: &str = "desktop-import";
const DESKTOP_TTL: i64 = 120_000;

pub fn start(params: &Value) -> Result<Value> {
    if !matches!(
        params["method"].as_str().unwrap_or(""),
        "" | "web" | "browser"
    ) {
        return Err(Error::http(400, "AutoClaw 国际版登录入口无效"));
    }
    match params["payload"]["methodIndex"].as_u64() {
        Some(0) | None => web_login::start(params, Vendor::Zai),
        Some(1) => Err(Error::http(
            400,
            "AutoClaw 国际版已移除 Google 登录，请改用 Zai 登录",
        )),
        Some(4) => desktop_start(params),
        _ => Err(Error::http(400, "手动或 JSON 凭据请通过对应账号表单提交")),
    }
}
fn desktop_start(params: &Value) -> Result<Value> {
    let flow = web_login::flow(params)?;
    let before = state::get(Scope::Login, DESKTOP, 1)?;
    if !before.compatible || !before.value.is_null() {
        return Err(Error::http(
            409,
            "AutoClaw 国际版本机导入已经发起或状态格式已变化",
        ));
    }
    let marker = util::random_hex(16)?;
    let expires = sdk::now() + DESKTOP_TTL;
    if !state::compare_exchange(
        Scope::Login,
        DESKTOP,
        1,
        before.revision,
        json!({ "phase": "reading", "flow": flow, "marker": marker, "expiresAt": expires }),
        DESKTOP_TTL,
    )? {
        return Err(Error::http(409, "AutoClaw 国际版导入流程已变化"));
    }
    let reserved = state::get(Scope::Login, DESKTOP, 1)?;
    if reserved.value["marker"] != marker || reserved.value["flow"] != flow {
        return Err(Error::http(409, "AutoClaw 国际版导入已取消"));
    }
    let result = desktop::read(None)
        .and_then(|found| desktop::reference(&found.secret))
        .and_then(account::normalized);
    match result {
        Ok(account) => {
            if !state::compare_exchange(
                Scope::Login,
                DESKTOP,
                1,
                reserved.revision,
                json!({ "phase": "ready", "flow": flow, "marker": marker, "account": account, "expiresAt": expires }),
                DESKTOP_TTL,
            )? {
                return Err(Error::http(409, "AutoClaw 国际版导入已取消，未添加账号"));
            }
        }
        Err(error) => {
            state::compare_exchange(
                Scope::Login,
                DESKTOP,
                1,
                reserved.revision,
                Value::Null,
                1000,
            )?;
            return Err(error);
        }
    }
    Ok(json!({ "flow": flow, "url": "", "poll_interval_ms": 1000,
        "instructions": "已读取国际版本机登录态，正在保存只读引用；不会修改官方文件。",
        "external_browser": false, "allowed_hosts": [],
        "login_ui": { "browserMode": "none", "manualCallback": false, "callbackInput": "url",
            "cookieCapture": null, "captureCallback": false } }))
}

pub fn poll(params: &Value) -> Result<Value> {
    let flow = web_login::flow(params)?;
    let before = state::get(Scope::Login, DESKTOP, 1)?;
    if before.value.is_null() {
        return web_login::poll(params);
    }
    if !before.compatible
        || before.value["flow"] != flow
        || before.value["expiresAt"].as_i64().unwrap_or(0) <= sdk::now()
    {
        cancel(params)?;
        return Ok(
            json!({ "state": "failed", "message": "AutoClaw 国际版本机导入已取消或过期，请重新发起" }),
        );
    }
    if before.value["phase"] == "reading" {
        return Ok(sdk::login::pending(None));
    }
    if before.value["phase"] != "ready" || !before.value["account"].is_object() {
        return Err(Error::http(409, "AutoClaw 国际版导入状态无效"));
    }
    if !state::compare_exchange(Scope::Login, DESKTOP, 1, before.revision, Value::Null, 1000)? {
        return Err(Error::http(409, "AutoClaw 国际版导入流程已变化"));
    }
    Ok(sdk::login::done(before.value["account"].clone()))
}

pub fn submit(params: &Value) -> Result<Value> {
    web_login::submit(params)
}
pub fn callback(params: &Value) -> Result<Value> {
    web_login::callback(params)
}
pub fn cancel(params: &Value) -> Result<Value> {
    let flow = web_login::flow(params)?;
    web_login::cancel(flow)?;
    for _ in 0..3 {
        let before = state::get(Scope::Login, DESKTOP, 1)?;
        if !before.value.is_null() && before.value["flow"] != flow {
            return Err(Error::http(409, "AutoClaw 国际版导入不属于当前流程"));
        }
        if state::compare_exchange(Scope::Login, DESKTOP, 1, before.revision, Value::Null, 1000)? {
            return Ok(Value::Null);
        }
    }
    Err(Error::http(
        409,
        "AutoClaw 国际版导入状态仍在变化，请重试取消",
    ))
}
