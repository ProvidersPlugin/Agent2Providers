use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};

use super::{account, upstream};

pub fn methods() -> Value {
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": "浏览器登录", "prompts": [],
          "browserMode": "choice", "manualCallback": false,
          "instructions": "在打开的 AtomGit 授权页完成登录；完成后回到这里等待几秒即可" },
        { "methodIndex": 1, "type": "api", "label": "手动填写令牌", "prompts": [
            { "key": "accessToken", "type": "text", "label": "Access Token", "secret": true },
            { "key": "refreshToken", "type": "text", "label": "Refresh Token", "secret": true, "optional": true },
            { "key": "userId", "type": "text", "label": "用户 ID", "optional": true }
          ], "instructions": "Refresh Token 可选；未填写时无法自动续期。" },
        { "methodIndex": 2, "type": "api", "label": "导入凭证 JSON", "prompts": [
            { "key": "credentialJson", "type": "text", "label": "凭证 JSON", "secret": true }
          ], "instructions": "粘贴 AtomCode 登录输出的 JSON，支持 access_token / refresh_token 字段。" }
    ] })
}

pub fn start(params: &Value) -> Result<Value> {
    if params["payload"]["methodIndex"].as_u64().unwrap_or(0) != 0 {
        return Err(Error::http(400, "此登录方式应通过账号表单提交"));
    }
    let url = format!("{}/auth/login?provider=atomgit", upstream::base("brokerBase"));
    let result = http::open("GET", &url, upstream::headers(None), None)?.json()?;
    let state = account::text(&result, &["state"]);
    let url = account::text(&result, &["login_url"]);
    if state.is_empty() || !url.starts_with("https://") || state.len() > 256 {
        return Err(Error::http(502, "AtomCode 未返回有效的授权地址与登录状态"));
    }
    let flow = account::text(params, &["flow"]);
    if flow.is_empty() { return Err(Error::new("主程序未分配登录流程")); }
    let snapshot = sdk::state::get(sdk::state::Scope::Login, "authorization", 1)?;
    if !sdk::state::compare_exchange(sdk::state::Scope::Login, "authorization", 1, snapshot.revision,
        json!({ "upstream_state": state }), 300_000)? { return Err(Error::new("登录状态已变化，请重新发起")); }
    Ok(json!({
        "flow": flow, "url": url, "poll_interval_ms": 3000,
        "instructions": "在打开的 AtomGit 授权页完成登录；完成后回到这里等待几秒即可",
        "external_browser": false, "allowed_hosts": [],
        "login_ui": { "browserMode": "choice", "manualCallback": false, "callbackInput": "url", "cookieCapture": null, "captureCallback": false }
    }))
}

pub fn poll(params: &Value) -> Result<Value> {
    let _ = params;
    let snapshot = sdk::state::get(sdk::state::Scope::Login, "authorization", 1)?;
    if !snapshot.compatible { return Err(Error::new("登录状态格式已变化，请重新发起")); }
    let state = account::text(&snapshot.value, &["upstream_state"]);
    if state.is_empty() { return Ok(json!({ "state": "failed", "message": "AtomCode 登录已取消或过期，请重新发起" })); }
    let base = upstream::base("brokerBase");
    let query = upstream::query(&state);
    let pending = || json!({ "state": "pending" });
    let check = match http::open("GET", &format!("{base}/auth/check?state={query}"), upstream::headers(None), None)
        .and_then(|response| response.json()) {
        Ok(value) => value,
        Err(_) => return Ok(pending()),
    };
    if check["valid"] != true { return Ok(pending()); }
    let response = http::open("GET", &format!("{base}/auth/token?state={query}"), upstream::headers(None), None)?.json()?;
    let secret = account::bundle(&response, None, true)?;
    sdk::state::compare_exchange(sdk::state::Scope::Login, "authorization", 1, snapshot.revision, Value::Null, 1000)?;
    Ok(json!({ "state": "done", "account": account::normalized(secret) }))
}
