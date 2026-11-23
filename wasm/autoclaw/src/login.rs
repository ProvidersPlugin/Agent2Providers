//! 旧 localOnly 无浏览器 flow 的兼容入口；新 desktop 方法直接走 account.normalize。
//! flow 中只保留不含 token 的实时引用；不宣称国内有官方网页登录。
use super::{account, desktop, sms, util};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    Error, Result, Value,
};

const KEY: &str = "desktop-import";

pub fn start(params: &Value) -> Result<Value> {
    if params["payload"]["methodIndex"].as_u64() != Some(3) {
        return Err(Error::http(
            400,
            "AutoClaw 国内版请使用手机验证码登录、填写凭据或本机导入",
        ));
    }
    let flow = params["flow"]
        .as_str()
        .filter(|flow| !flow.is_empty() && flow.len() <= 256 && !flow.contains('\0'))
        .ok_or_else(|| Error::http(400, "宿主未分配 AutoClaw 导入流程"))?;
    let before = state::get(Scope::Login, KEY, 1)?;
    let marker = util::random_hex(16)?;
    if !state::compare_exchange(
        Scope::Login,
        KEY,
        1,
        before.revision,
        json!({ "phase": "reading", "marker": marker }),
        120_000,
    )? {
        return Err(Error::http(409, "AutoClaw 导入流程已变化"));
    }
    let reserved = state::get(Scope::Login, KEY, 1)?;
    if reserved.value["marker"] != marker {
        return Err(Error::http(409, "AutoClaw 导入已取消"));
    }
    let result = desktop::read(None)
        .and_then(|found| desktop::reference(&found.secret))
        .and_then(account::normalized);
    match result {
        Ok(account) => {
            if !state::compare_exchange(
                Scope::Login,
                KEY,
                1,
                reserved.revision,
                json!({ "phase": "ready", "account": account, "expiresAt": sdk::now() + 120_000 }),
                120_000,
            )? {
                return Err(Error::http(409, "AutoClaw 导入已取消或更新，未添加账号"));
            }
        }
        Err(error) => {
            state::compare_exchange(Scope::Login, KEY, 1, reserved.revision, Value::Null, 1000)?;
            return Err(error);
        }
    }
    Ok(json!({ "flow": flow, "url": "", "poll_interval_ms": 1000,
        "instructions": "已读取 AutoClaw 本机登录态，正在保存只读引用。", "external_browser": false, "allowed_hosts": [],
        "login_ui": { "browserMode": "none", "manualCallback": false, "callbackInput": "url", "cookieCapture": null, "captureCallback": false } }))
}

pub fn poll() -> Result<Value> {
    let before = state::get(Scope::Login, KEY, 1)?;
    if !before.compatible
        || before.value["phase"] != "ready"
        || before.value["expiresAt"].as_i64().unwrap_or(0) <= sdk::now()
    {
        return Ok(
            json!({ "state": "failed", "message": "AutoClaw 本机导入已取消或过期，请重新发起" }),
        );
    }
    if !state::compare_exchange(Scope::Login, KEY, 1, before.revision, Value::Null, 1000)? {
        return Err(Error::http(409, "AutoClaw 导入流程已变化"));
    }
    Ok(sdk::login::done(before.value["account"].clone()))
}

pub fn cancel() -> Result<Value> {
    sms::cancel()?;
    for _ in 0..3 {
        let before = state::get(Scope::Login, KEY, 1)?;
        if state::compare_exchange(Scope::Login, KEY, 1, before.revision, Value::Null, 1000)? {
            return Ok(Value::Null);
        }
    }
    Err(Error::http(409, "AutoClaw 导入状态仍在变化，请重试取消"))
}
