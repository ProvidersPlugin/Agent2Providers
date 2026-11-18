use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};

use super::{account, upstream};

pub fn methods() -> Value {
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": "浏览器登录", "prompts": [],
          "browserMode": "choice", "manualCallback": false,
          "instructions": "在打开的 AtomGit 授权页完成登录；完成后回到这里等待几秒即可。" },
        { "methodIndex": 1, "type": "api", "label": "手动填写令牌", "prompts": [
            { "key": "accessToken", "type": "text", "label": "Access Token", "secret": true },
            { "key": "refreshToken", "type": "text", "label": "Refresh Token", "secret": true, "optional": true },
            { "key": "userId", "type": "text", "label": "用户 ID", "optional": true }
          ], "instructions": "Refresh Token 可选；未填写时无法自动续期。用户 ID 未填时会通过官方只读资料接口补全；未取得身份前不会发送对话。" },
        { "methodIndex": 2, "type": "api", "label": "导入凭证 JSON", "prompts": [
            { "key": "credentialJson", "type": "text", "label": "凭证 JSON", "secret": true }
          ], "instructions": "粘贴 AtomCode 登录输出的 JSON，支持 access_token / refresh_token 与 user.id 字段；缺少用户身份时会只读补全。" }
    ] })
}

pub fn start(params: &Value) -> Result<Value> {
    if params["payload"]["methodIndex"].as_u64().unwrap_or(0) != 0 {
        return Err(Error::http(400, "此登录方式应通过账号表单提交"));
    }
    let url = format!("{}/auth/login?provider=atomgit", upstream::base("brokerBase"));
    let result = upstream::response_json(http::open_timeout("GET", &url, upstream::headers(None), None, 20_000)?)?;
    let state = account::text(&result, &["state"]);
    let raw_url = account::text(&result, &["login_url"]);
    let url = url::Url::parse(&raw_url).ok().filter(|url| {
        let host = url.host_str().unwrap_or("");
        url.scheme() == "https" && url.port_or_known_default() == Some(443)
            && (host == "atomgit.com" || host.ends_with(".atomgit.com"))
            && url.username().is_empty() && url.password().is_none()
    }).ok_or_else(|| Error::http(502, "AtomCode 未返回可信的 AtomGit 授权地址"))?;
    if state.is_empty() || state.len() > 256 || state.chars().any(char::is_control) {
        return Err(Error::http(502, "AtomCode 未返回有效的登录状态"));
    }
    let flow = account::text(params, &["flow"]);
    if flow.is_empty() { return Err(Error::new("主程序未分配登录流程")); }
    let snapshot = sdk::state::get(sdk::state::Scope::Login, "authorization", 1)?;
    if !snapshot.compatible || !sdk::state::compare_exchange(sdk::state::Scope::Login, "authorization", 1, snapshot.revision,
        json!({ "upstream_state": state }), 300_000)? { return Err(Error::new("登录状态已变化，请重新发起")); }
    Ok(json!({
        "flow": flow, "url": url.as_str(), "poll_interval_ms": 3000,
        "instructions": "在打开的 AtomGit 授权页完成登录；完成后回到这里等待几秒即可。",
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
    let check = match http::open_timeout("GET", &format!("{base}/auth/check?state={query}"), upstream::headers(None), None, 15_000)
        .and_then(upstream::response_json) {
        Ok(value) => value,
        Err(error) if matches!(error.status, Some(400 | 401 | 403 | 404 | 410)) => {
            return Ok(json!({ "state": "failed", "message": "AtomCode 授权状态已失效或被上游拒绝，请重新发起登录" }));
        }
        Err(_) => return Ok(pending()),
    };
    if check["valid"] != true { return Ok(pending()); }
    let response = upstream::response_json(http::open_timeout("GET", &format!("{base}/auth/token?state={query}"),
        upstream::headers(None), None, 20_000)?)?;
    let mut secret = account::bundle(&response, None, true)?;
    account::fill_profile(&mut secret)?;
    if !sdk::state::compare_exchange(sdk::state::Scope::Login, "authorization", 1, snapshot.revision, Value::Null, 1000)? {
        return Err(Error::new("AtomCode 登录在取回凭据期间已取消或变化，未保存迟到账号"));
    }
    Ok(json!({ "state": "done", "account": account::normalized(secret) }))
}

pub fn cancel() -> Result<Value> {
    let snapshot = sdk::state::get(sdk::state::Scope::Login, "authorization", 1)?;
    if !sdk::state::compare_exchange(sdk::state::Scope::Login, "authorization", 1, snapshot.revision, Value::Null, 1000)? {
        return Err(Error::new("AtomCode 登录状态已变化"));
    }
    Ok(Value::Null)
}
