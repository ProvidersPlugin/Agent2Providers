//! WorkOS设备授权→Cline注册；device_code和令牌不作为公开flow。
//! 每次只轮询一次，成功后立即换Cline会话；间隔、slow_down与到期时间写flow隔离状态。

use super::{account, client, endpoints, i18n};
use agent2api_wasm_sdk::{self as sdk, json, state::Scope, Error, Result, Value};
use url::Url;

const KEY: &str = "device-authorization";
const TTL: i64 = 300_000;
// 普通登录单次宿主预算为 120 秒；迟到步骤不重兑，只结束本次等待。
const STEP_TTL: i64 = 125_000;

fn flow(params: &Value) -> Result<&str> {
    params["flow"]
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .ok_or_else(|| Error::http(400, "主程序未分配 Cline 登录流程"))
}

fn verification(value: &str) -> Result<String> {
    let url = Url::parse(value).map_err(|_| Error::http(502, "Cline 未提供有效设备授权地址"))?;
    if url.scheme() != "https"
        || !matches!(url.host_str(), Some("authkit.cline.bot" | "api.workos.com"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
        || url.fragment().is_some()
    {
        return Err(Error::http(502, "Cline 设备授权地址不在已确认的站点中"));
    }
    Ok(url.to_string())
}

pub fn start(params: &Value) -> Result<Value> {
    if params["payload"]
        .get("methodIndex")
        .is_some_and(|index| index.as_u64() != Some(0))
    {
        return Err(Error::http(400, "Cline 手工凭据请通过账号表单提交"));
    }
    let flow = flow(params)?;
    let locale = i18n::locale(params["payload"]["locale"].as_str().unwrap_or("zh-CN"));
    let reply = client::form(
        "/user_management/authorize/device",
        &[("client_id", endpoints::CLIENT_ID)],
    )?;
    if !(200..300).contains(&reply.status) {
        return Err(Error::http(502, "Cline 设备授权启动失败，请稍后重试"));
    }
    let device_code = account::text(&reply.body, &["device_code"]);
    let user_code = account::text(&reply.body, &["user_code"]);
    if device_code.is_empty()
        || device_code.len() > 8192
        || device_code.chars().any(char::is_control)
        || user_code.is_empty()
        || user_code.len() > 128
        || user_code.chars().any(char::is_control)
    {
        return Err(Error::http(502, "Cline 设备授权返回的代码格式不正确"));
    }
    let candidate = account::text(
        &reply.body,
        &["verification_uri_complete", "verification_uri"],
    );
    let url = verification(if candidate.is_empty() {
        endpoints::VERIFY
    } else {
        &candidate
    })?;
    let expires = reply.body["expires_in"]
        .as_i64()
        .unwrap_or(300)
        .clamp(1, 300)
        * 1000;
    let interval = reply.body["interval"].as_i64().unwrap_or(5).clamp(1, 60) * 1000;
    let current = sdk::state::get(Scope::Login, KEY, 1)?;
    if !current.compatible
        || !sdk::state::compare_exchange(
            Scope::Login,
            KEY,
            1,
            current.revision,
            json!({ "device_code": device_code, "expires_at": sdk::now().saturating_add(expires), "interval": interval,
            "next_at": sdk::now().saturating_add(interval), "phase": "waiting", "locale": locale }),
            TTL,
        )?
    {
        return Err(Error::http(409, "Cline 登录状态已变化，请重新发起"));
    }
    Ok(
        json!({ "flow": flow, "url": url, "poll_interval_ms": interval,
        "instructions": i18n::login_prompt(locale, &user_code, expires / 1000),
        "external_browser": true, "allowed_hosts": ["authkit.cline.bot", "api.workos.com"],
        "login_ui": { "browserMode": "external", "manualCallback": false, "callbackInput": "url", "cookieCapture": null, "captureCallback": false } }),
    )
}

fn pending(locale: &str) -> Value {
    json!({ "state": "pending", "hint": i18n::login_pending(locale) })
}
fn failed(locale: &str, kind: &str) -> Value {
    json!({ "state": "failed", "message": i18n::login_failure(locale, kind) })
}

fn replace(revision: u64, value: Value) -> Result<()> {
    if !sdk::state::compare_exchange(Scope::Login, KEY, 1, revision, value, TTL)? {
        return Err(Error::http(409, "Cline 登录步骤已变化，请勿重复提交"));
    }
    Ok(())
}

pub fn poll(params: &Value) -> Result<Value> {
    flow(params)?;
    let snapshot = sdk::state::get(Scope::Login, KEY, 1)?;
    let state = snapshot.value;
    let locale = i18n::locale(state["locale"].as_str().unwrap_or("zh-CN"));
    if !snapshot.compatible || state["phase"].as_str().is_none() {
        return Ok(failed(locale, "canceled"));
    }
    if state["expires_at"]
        .as_i64()
        .is_none_or(|at| at <= sdk::now())
    {
        replace(snapshot.revision, Value::Null)?;
        return Ok(failed(locale, "expired"));
    }
    match state["phase"].as_str() {
        Some("waiting") => {}
        Some("polling" | "registering") => {
            // 一次性兑换结果未知不能永远等待，更不能重新消费同一设备码。
            if state["step_until"]
                .as_i64()
                .is_none_or(|until| until <= sdk::now())
            {
                replace(snapshot.revision, Value::Null)?;
                return Ok(failed(locale, "unknown"));
            }
            return Ok(pending(locale));
        }
        _ => return Ok(failed(locale, "canceled")),
    }
    if state["next_at"].as_i64().is_some_and(|at| at > sdk::now()) {
        return Ok(pending(locale));
    }
    let mut running = state.clone();
    running["phase"] = json!("polling");
    running["step_until"] = json!(sdk::now().saturating_add(STEP_TTL));
    if !sdk::state::compare_exchange(Scope::Login, KEY, 1, snapshot.revision, running, TTL)? {
        return Ok(pending(locale));
    }
    let device = account::text(&state, &["device_code"]);
    let response = client::form(
        "/user_management/authenticate",
        &[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", &device),
            ("client_id", endpoints::CLIENT_ID),
        ],
    );
    let current = sdk::state::get(Scope::Login, KEY, 1)?;
    // 取消后迟到的回应不能重建流程。
    if !current.compatible || current.value["phase"] != "polling" {
        return Ok(failed(locale, "canceled"));
    }
    if current.value["expires_at"]
        .as_i64()
        .is_none_or(|at| at <= sdk::now())
    {
        replace(current.revision, Value::Null)?;
        return Ok(failed(locale, "expired"));
    }
    let reply = match response {
        Ok(reply) => reply,
        Err(_) => {
            replace(current.revision, Value::Null)?;
            return Ok(failed(locale, "transport"));
        }
    };
    if !(200..300).contains(&reply.status) {
        let code = account::text(&reply.body, &["error"]).to_ascii_lowercase();
        if matches!(code.as_str(), "authorization_pending" | "slow_down") {
            let mut next = state;
            let interval = next["interval"].as_i64().unwrap_or(5000);
            let interval = if code == "slow_down" {
                interval.saturating_add(5000).min(60_000)
            } else {
                interval
            };
            next["interval"] = json!(interval);
            next["next_at"] = json!(sdk::now().saturating_add(interval));
            replace(current.revision, next)?;
            return Ok(pending(locale));
        }
        replace(current.revision, Value::Null)?;
        return Ok(failed(locale, &code));
    }
    let credentials = (|| -> Result<(String, String)> {
        let access = account::clean_token(&account::text(&reply.body, &["access_token"]))?;
        let refresh = account::clean_token(&account::text(&reply.body, &["refresh_token"]))?;
        if access.is_empty() || refresh.is_empty() {
            return Err(Error::http(502, "Cline 设备授权响应缺少令牌"));
        }
        Ok((access, refresh))
    })();
    let (access, refresh) = match credentials {
        Ok(credentials) => credentials,
        Err(_) => {
            replace(current.revision, Value::Null)?;
            return Ok(failed(locale, "transport"));
        }
    };
    let mut registering = current.value;
    registering["phase"] = json!("registering");
    registering["step_until"] = json!(sdk::now().saturating_add(STEP_TTL));
    // 不保存WorkOS令牌；注册阶段失败必须重新发起，避免重兑结果不明的凭证。
    replace(current.revision, registering)?;
    let registered = (|| -> Result<Value> {
        let reply = client::request(
            "POST",
            "/auth/register",
            None,
            true,
            Some(&json!({ "accessToken": access, "refreshToken": refresh })),
        )?;
        let data = reply.data()?;
        let access = account::access(&data)
            .map_err(|_| Error::http(502, "Cline 令牌登记响应缺少有效会话凭证"))?;
        let mut credentials = data.clone();
        credentials["accessToken"] = json!(access);
        if account::text(&data, &["refreshToken", "refresh_token"]).is_empty() {
            credentials["refreshToken"] = json!(refresh);
        }
        let secret = account::parse(&credentials)?;
        account::normalized(secret, None)
    })();
    let current = sdk::state::get(Scope::Login, KEY, 1)?;
    if !current.compatible || current.value["phase"] != "registering" {
        return Ok(failed(locale, "canceled"));
    }
    let expired = current.value["expires_at"]
        .as_i64()
        .is_none_or(|at| at <= sdk::now());
    replace(current.revision, Value::Null)?;
    if expired {
        return Ok(failed(locale, "expired"));
    }
    Ok(match registered {
        Ok(account) => json!({ "state": "done", "account": account }),
        Err(_) => failed(locale, "register"),
    })
}

pub fn cancel() -> Result<Value> {
    let state = sdk::state::get(Scope::Login, KEY, 1)?;
    replace(state.revision, Value::Null)?;
    Ok(Value::Null)
}
