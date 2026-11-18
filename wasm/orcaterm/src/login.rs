//! 官方 session_id 换码；尚未完成时 TOKEN_EXPIRED 表示 pending。
//! 回调和轮询共享同一状态 CAS，已返回的令牌先保存再补身份，取消不能复活流程。

use super::{account, credentials, errors, upstream};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope, Snapshot},
    Error, Result, Value,
};

const STATE_KEY: &str = "wechat-authorization";
const SCHEMA: u32 = 1;
const TTL_MS: i64 = 5 * 60 * 1000;
const CALLBACK: &str = "orcaterm://oauth_callback";
const ALLOWED_HOSTS: &[&str] = &["orcaterm.com", "open.weixin.qq.com", "wx.qq.com"];

fn snapshot() -> Result<Snapshot> {
    let current = state::get(Scope::Login, STATE_KEY, SCHEMA)?;
    if !current.compatible {
        return Err(Error::new("OrcaTerm 登录状态格式已变化，请重新发起"));
    }
    Ok(current)
}
fn save(previous: &Snapshot, value: Value) -> Result<bool> {
    state::compare_exchange(
        Scope::Login,
        STATE_KEY,
        SCHEMA,
        previous.revision,
        value,
        TTL_MS,
    )
}
fn failed(message: &str) -> Value {
    json!({ "state": "failed", "message": message })
}
fn pending(hint: &str) -> Value {
    json!({ "state": "pending", "hint": hint })
}

pub fn start(params: &Value) -> Result<Value> {
    if params["payload"]["methodIndex"].as_u64().unwrap_or(0) != 0 {
        return Err(Error::http(400, "OrcaTerm 手动凭据请通过添加账号表单提交"));
    }
    if params["method"]
        .as_str()
        .is_some_and(|method| !matches!(method, "" | "browser"))
    {
        return Err(Error::http(400, "OrcaTerm 当前登录入口只接受微信扫码授权"));
    }
    let flow = params["flow"]
        .as_str()
        .filter(|flow| !flow.is_empty())
        .ok_or_else(|| Error::new("主程序未分配 OrcaTerm 登录流程"))?;
    let session_id = credentials::random_uuid()?;
    let previous = snapshot()?;
    let name = params["payload"]["inputs"]["name"]
        .as_str()
        .or_else(|| params["payload"]["name"].as_str())
        .unwrap_or("")
        .trim();
    if !save(
        &previous,
        json!({ "phase": "waiting", "sessionId": session_id,
        "expiresAt": sdk::now().saturating_add(TTL_MS), "name": name, "nextPollAt": 0 }),
    )? {
        return Err(Error::new("OrcaTerm 登录状态已变化，请重新发起"));
    }
    let external = params["payload"]["mode"] == "external";
    // 外部浏览器回官网，不把登录交给官方已安装程序的自定义协议处理器。
    let callback = if external {
        "https://orcaterm.com/"
    } else {
        CALLBACK
    };
    let url = format!("https://orcaterm.com/oauth/authorize?provider=wechat&source=desktop&session_id={session_id}&return_url={}", upstream::encode(callback));
    Ok(json!({ "flow": flow, "url": url, "poll_interval_ms": 3000,
        "instructions": "在 OrcaTerm 官方授权页用微信扫码，完成后自动回填账号，无需复制回调地址。",
        "external_browser": external, "allowed_hosts": ALLOWED_HOSTS,
        "login_ui": { "browserMode": "choice", "manualCallback": false, "callbackInput": "url",
            "cookieCapture": null, "captureCallback": !external } }))
}

fn status(current: &Snapshot) -> Option<Value> {
    match current.value["phase"].as_str() {
        Some("done") => Some(sdk::login::done(current.value["account"].clone())),
        Some("failed") => Some(failed(
            current.value["message"]
                .as_str()
                .unwrap_or("OrcaTerm 登录失败，请重新发起"),
        )),
        Some("cancelled") => Some(failed("OrcaTerm 登录已取消")),
        _ if current.value.is_null()
            || current.value["expiresAt"].as_i64().unwrap_or(0) <= sdk::now() =>
        {
            Some(failed("OrcaTerm 登录会话已过期，请重新发起扫码"))
        }
        Some("exchanging") => Some(pending(
            "正在完成 OrcaTerm 授权；已发出的换码请求未确认时不会重复提交",
        )),
        _ => None,
    }
}

pub fn poll() -> Result<Value> {
    let current = snapshot()?;
    if let Some(result) = status(&current) {
        return Ok(result);
    }
    if current.value["phase"] == "verifying" {
        return verify(&current);
    }
    if current.value["nextPollAt"].as_i64().unwrap_or(0) > sdk::now() {
        return Ok(pending("等待微信授权完成"));
    }
    exchange(&current)
}

fn callback_session(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if valid_uuid(raw) {
        return Some(raw.to_string());
    }
    let (base, query) = raw.split_once('?')?;
    if !base.eq_ignore_ascii_case(CALLBACK) && !base.eq_ignore_ascii_case(&format!("{CALLBACK}/")) {
        return None;
    }
    if query.contains('#') {
        return None;
    }
    let mut values = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(key, _)| *key == "session_id");
    let (_, value) = values.next()?;
    if values.next().is_some() || !valid_uuid(value) {
        return None;
    }
    Some(value.to_string())
}
fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

pub fn callback(params: &Value) -> Result<Value> {
    let current = snapshot()?;
    if let Some(result) = status(&current) {
        return Ok(result);
    }
    let supplied = params["url"]
        .as_str()
        .filter(|raw| !raw.trim().is_empty())
        .or_else(|| params["code"].as_str())
        .unwrap_or("");
    if callback_session(supplied).as_deref() != current.value["sessionId"].as_str() {
        let mut result = pending("回调与本次 OrcaTerm 登录不匹配，请等待本次官方授权完成");
        result["retryable"] = json!(true);
        return Ok(result);
    }
    if current.value["phase"] == "verifying" {
        verify(&current)
    } else {
        exchange(&current)
    }
}

fn exchange(previous: &Snapshot) -> Result<Value> {
    let session_id = previous.value["sessionId"]
        .as_str()
        .filter(|value| valid_uuid(value))
        .ok_or_else(|| Error::new("OrcaTerm 本次授权标识无效"))?;
    let mut exchanging = previous.value.clone();
    exchanging["phase"] = json!("exchanging");
    if !save(previous, exchanging)? {
        return Ok(pending("OrcaTerm 登录步骤正在更新"));
    }
    let reserved = snapshot()?;
    if reserved.value["phase"] != "exchanging"
        || reserved.value["sessionId"].as_str() != Some(session_id)
    {
        return Ok(pending("OrcaTerm 登录步骤已变化"));
    }
    let response = upstream::cgi(
        None,
        "OAuthExchangeToken",
        json!({ "sessionId": session_id }),
    );
    let mut next = reserved.value.clone();
    match response {
        Ok(response) if response["Error"]["Code"] == "TOKEN_EXPIRED" => {
            next["phase"] = json!("waiting");
            next["nextPollAt"] = json!(sdk::now().saturating_add(3000));
        }
        Ok(response) => {
            if errors::business_failed(&response) {
                next["phase"] = json!("failed");
                next["message"] = json!("OrcaTerm 授权被拒绝，请重新发起扫码");
            } else {
                let token = credentials::text(&response, &["AccessToken", "accessToken"]);
                match credentials::from_payload(&json!(token)) {
                    Ok(mut credentials) => {
                        if credentials.expires_at <= 0 {
                            credentials.expires_at = response["ExpiresIn"]
                                .as_i64()
                                .filter(|seconds| *seconds > 0)
                                .map(|seconds| (sdk::now() / 1000).saturating_add(seconds))
                                .unwrap_or(0);
                        }
                        // 先保存已签发材料，探活抖动不能再消费一次性 session_id。
                        next["phase"] = json!("verifying");
                        next["credentials"] = credentials.secret();
                    }
                    Err(_) => {
                        next["phase"] = json!("failed");
                        next["message"] = json!("OrcaTerm 授权响应缺少有效令牌，请重新扫码");
                    }
                }
            }
        }
        Err(_) => {
            next["phase"] = json!("failed");
            next["message"] = json!("OrcaTerm 换码请求未能确认，请重新扫码；未重复提交一次性授权");
        }
    }
    if !save(&reserved, next)? {
        return Ok(pending("OrcaTerm 登录步骤已变化"));
    }
    let current = snapshot()?;
    if current.value["phase"] == "verifying" {
        verify(&current)
    } else {
        Ok(status(&current).unwrap_or_else(|| pending("等待微信授权完成")))
    }
}

fn verify(previous: &Snapshot) -> Result<Value> {
    let mut credentials = credentials::from_secret(&previous.value["credentials"])?;
    let mut next = previous.value.clone();
    match upstream::user_info(&credentials) {
        Ok((uid, nickname)) => {
            if account::bind_profile(&mut credentials, uid, nickname).is_err() {
                next["phase"] = json!("failed");
                next["message"] = json!("OrcaTerm 返回的账号与本次授权不一致，请重新扫码");
                if save(previous, next)? {
                    return Ok(failed("OrcaTerm 返回的账号与本次授权不一致，请重新扫码"));
                }
                return Ok(pending("OrcaTerm 登录步骤已变化"));
            }
        }
        Err(error) if errors::is_auth(&error) => {
            next["phase"] = json!("failed");
            next["message"] = json!("OrcaTerm 已签发登录态被拒绝，请重新扫码");
            if save(previous, next)? {
                return Ok(failed("OrcaTerm 已签发登录态被拒绝，请重新扫码"));
            }
            return Ok(pending("OrcaTerm 登录步骤已变化"));
        }
        Err(_) => {}
    }
    if credentials.user_id.is_empty() {
        return Ok(pending("OrcaTerm 已完成授权，正在核对账号身份"));
    }
    let account = account::normalized(&credentials, previous.value["name"].as_str(), false)?;
    // done 不再保留 session_id 或中间凭据；只有 CAS 成功的本次流程可返回账号。
    if !save(previous, json!({ "phase": "done", "account": account }))? {
        return Ok(pending("OrcaTerm 登录步骤已变化"));
    }
    Ok(sdk::login::done(account))
}

pub fn cancel() -> Result<Value> {
    for _ in 0..3 {
        let current = snapshot()?;
        if save(&current, json!({ "phase": "cancelled" }))? {
            return Ok(Value::Null);
        }
    }
    Err(Error::new("OrcaTerm 登录状态正被更新，未覆盖其他操作"))
}
