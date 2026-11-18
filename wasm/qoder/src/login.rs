//! 国内网页登录是设备授权 + S256 PKCE，无授权码/localhost 回调、无客户端文件或 UMID。
//! nonce 保留 UUID 横线（去掉会使当前四站接口拒绝）；verifier 仅在宿主 flow 隔离状态。
use super::{account, auth, client, credentials::Credentials, endpoints, login_methods, util};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    Error, Result, Value,
};
use base64::Engine;
use sha2::{Digest, Sha256};

const KEY: &str = "device-login";
const TTL: i64 = 300_000;
fn flow(params: &Value) -> Result<&str> {
    params["flow"]
        .as_str()
        .filter(|flow| !flow.is_empty() && util::printable(flow, 256))
        .ok_or_else(|| Error::http(400, "主程序未分配 Qoder 登录流程"))
}
fn changed() -> Error {
    Error::http(409, "Qoder 登录已取消、过期或被替换")
}
fn failed(message: &str) -> Value {
    json!({ "state": "failed", "message": message, "interaction": null })
}
fn put(before: &state::Snapshot, value: Value, ttl: i64) -> Result<()> {
    if !state::compare_exchange(Scope::Login, KEY, 1, before.revision, value, ttl)? {
        return Err(changed());
    }
    Ok(())
}
pub fn start(params: &Value) -> Result<Value> {
    if !matches!(
        params["method"].as_str().unwrap_or(""),
        "" | "browser" | "web"
    ) || params["payload"]["methodIndex"].as_u64().unwrap_or(0) != 0
    {
        return Err(Error::http(
            400,
            "Qoder 手动凭据请使用账号表单，设备授权请使用网页登录",
        ));
    }
    let flow = flow(params)?;
    let before = state::get(Scope::Login, KEY, 1)?;
    if !before.value.is_null() {
        return Err(changed());
    }
    let verifier = util::random_secret()?;
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let nonce = util::uuid()?;
    // 原生对普通 machineId 允许随机 UUID；保存在此 flow，成功后随账号落地。
    let machine = util::uuid()?;
    let now = util::now()?;
    let mut url = url::Url::parse(&format!("{}{}", endpoints::WEB, endpoints::DEVICE_LOGIN))
        .map_err(|_| Error::new("Qoder 授权地址配置无效"))?;
    url.query_pairs_mut()
        .append_pair("challenge", &challenge)
        .append_pair("challenge_method", "S256")
        .append_pair("machine_id", &machine)
        .append_pair("nonce", &nonce);
    // Qoder 中国版原生不发 client_id/redirect_uri；不套千问办公的那组字段。
    put(
        &before,
        json!({ "flow": flow, "nonce": nonce, "verifier": verifier, "machineId": machine,
        "phase": "waiting", "expiresAt": now + TTL, "nextAt": now, "generation": util::uuid()? }),
        TTL,
    )?;
    Ok(
        json!({ "flow": flow, "url": url.to_string(), "poll_interval_ms": 2000,
        "instructions": login_methods::instructions(params["payload"]["locale"].as_str().unwrap_or("zh-CN")),
        "external_browser": true, "allowed_hosts": ["qoder.com.cn", "qoder.com", "qoder.sh"],
        "login_ui": { "browserMode": "external", "manualCallback": false, "callbackInput": "url",
            "cookieCapture": null, "captureCallback": false } }),
    )
}
pub fn poll(params: &Value) -> Result<Value> {
    let flow = flow(params)?;
    let before = state::get(Scope::Login, KEY, 1)?;
    let now = util::now()?;
    if !before.value.is_null() && before.value["flow"] != flow {
        return Err(changed());
    }
    if !before.compatible
        || before.value.is_null()
        || before.value["expiresAt"].as_i64().unwrap_or(0) <= now
    {
        put(&before, Value::Null, 1000)?;
        return Ok(failed("Qoder 设备授权已取消或过期，请重新发起"));
    }
    if before.value["phase"] == "polling" {
        if before.value["busyUntil"].as_i64().unwrap_or(0) <= now {
            put(&before, Value::Null, 1000)?;
            return Ok(failed(
                "Qoder 上轮授权结果未确认，请重新发起，未重复领取设备凭据",
            ));
        }
        return Ok(sdk::login::pending(None));
    }
    if before.value["phase"] != "waiting" {
        return Err(changed());
    }
    if before.value["nextAt"].as_i64().unwrap_or(0) > now {
        return Ok(sdk::login::pending(None));
    }
    let mut working = before.value.clone();
    let marker = util::uuid()?;
    working["phase"] = json!("polling");
    working["marker"] = json!(marker);
    working["busyUntil"] = json!(now + 120_000);
    let ttl = working["expiresAt"]
        .as_i64()
        .unwrap_or(now)
        .saturating_sub(now)
        .min(TTL);
    put(&before, working.clone(), ttl)?;
    let reserved = state::get(Scope::Login, KEY, 1)?;
    if reserved.value["marker"] != marker || reserved.value["flow"] != flow {
        return Err(changed());
    }
    let result = step(&working);
    if working["expiresAt"].as_i64().unwrap_or(0) <= util::now()? {
        put(&reserved, Value::Null, 1000)?;
        return Ok(failed("Qoder 设备授权已过期，未添加迟到的账号"));
    }
    match result {
        Ok(Some(credentials)) => {
            let result = account::normalized(credentials, "web", "");
            if working["expiresAt"].as_i64().unwrap_or(0) <= util::now()? {
                put(&reserved, Value::Null, 1000)?;
                return Ok(failed("Qoder 设备授权已过期，未添加迟到的账号"));
            }
            put(&reserved, Value::Null, 1000)?;
            result.map(sdk::login::done)
        }
        Ok(None) => {
            working["phase"] = json!("waiting");
            working["nextAt"] = json!(util::now()? + 2000);
            working["busyUntil"] = json!(0);
            put(&reserved, working, ttl)?;
            Ok(sdk::login::pending(None))
        }
        Err(error) if error.code.as_deref() == Some("qoder_transport") => {
            working["phase"] = json!("waiting");
            working["nextAt"] = json!(util::now()? + 2000);
            working["busyUntil"] = json!(0);
            put(&reserved, working, ttl)?;
            Ok(json!({ "state": "pending", "hint": "Qoder 设备授权服务暂时不可达，正在等待重试" }))
        }
        Err(error) => {
            put(&reserved, Value::Null, 1000)?;
            if util::terminal(&error) {
                return Err(error);
            }
            Ok(failed(&error.message))
        }
    }
}
fn step(value: &Value) -> Result<Option<Credentials>> {
    let mut url = url::Url::parse(&format!(
        "{}{}",
        endpoints::OPEN_API,
        endpoints::DEVICE_POLL
    ))
    .map_err(|_| Error::new("Qoder 轮询地址配置无效"))?;
    let nonce = value["nonce"].as_str().ok_or_else(changed)?;
    let verifier = value["verifier"].as_str().ok_or_else(changed)?;
    let machine = value["machineId"].as_str().ok_or_else(changed)?;
    url.query_pairs_mut()
        .append_pair("nonce", nonce)
        .append_pair("verifier", verifier)
        .append_pair("challenge_method", "S256");
    let path = format!("{}?{}", url.path(), url.query().unwrap_or(""));
    let reply = client::request(
        "GET",
        endpoints::OPEN_API,
        &path,
        client::account_headers(None),
        None,
        "设备授权轮询",
    )?;
    if matches!(reply.status, 202 | 404) {
        return Ok(None);
    }
    let data = client::accepted(reply, "设备授权")?;
    let access = util::secret(&data, &["token", "device_token"])?;
    let refresh = util::secret(&data, &["refresh_token"])?;
    if access.is_empty() || access.contains('|') || refresh.contains('|') {
        return Err(Error::http(502, "Qoder 设备授权未返回有效凭据"));
    }
    let user = util::text(&data, &["user_id", "uid"]);
    let mut credentials = Credentials::parse(
        &json!({ "mode": "cn", "accessToken": access, "refreshToken": refresh,
        "userId": user, "machineId": machine, "expiresAt": util::timestamp(&data["expires_at"]).unwrap_or(util::now()? + endpoints::DEVICE_LIFETIME) }),
    )?;
    if let Err(error) = auth::profile(&mut credentials) {
        if credentials.user.is_empty()
            || util::terminal(&error)
            || matches!(error.status, Some(401 | 403))
        {
            return Err(if error.code.as_deref() == Some("qoder_transport") {
                Error::http(502, "Qoder 已返回设备凭据但未能确认用户身份，请重新授权")
            } else {
                error
            });
        }
    }
    credentials.complete()?;
    Ok(Some(credentials))
}
pub fn cancel(params: &Value) -> Result<Value> {
    let flow = flow(params)?;
    for _ in 0..3 {
        let before = state::get(Scope::Login, KEY, 1)?;
        if !before.value.is_null() && before.value["flow"] != flow {
            return Err(changed());
        }
        if state::compare_exchange(Scope::Login, KEY, 1, before.revision, Value::Null, 1000)? {
            return Ok(Value::Null);
        }
    }
    Err(changed())
}
