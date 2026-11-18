//! 千问办公国内设备授权：S256 PKCE + 带横线 UUID nonce + 国内 client_id/redirect_uri。
//! 轮询取凭据，不监听本机回调；缺机器编号的 UUID 只生成一次，flow 内保存并随账号落地。
use super::{
    account, auth, client,
    credentials::{self, Credentials},
    endpoints, login_methods, machine, util,
};
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
        .ok_or_else(|| Error::http(400, "主程序未分配千问办公登录流程"))
}
fn changed() -> Error {
    Error::http(409, "千问办公登录已取消、过期或被替换")
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
    let locale = params["payload"]["locale"].as_str().unwrap_or("zh-CN");
    if !matches!(locale, "zh-CN" | "zh-Hant" | "en" | "ja" | "ko" | "pt-BR") {
        return Err(Error::http(400, "千问办公登录语言无效"));
    }
    if !matches!(
        params["method"].as_str().unwrap_or(""),
        "" | "browser" | "web"
    ) || params["payload"]
        .get("methodIndex")
        .is_some_and(|index| index.as_u64() != Some(0))
    {
        return Err(Error::http(
            400,
            "千问办公手工凭据请使用账号表单，设备授权请使用网页登录",
        ));
    }
    if params["payload"]
        .get("mode")
        .is_some_and(|mode| !matches!(mode.as_str(), Some("external" | "")))
    {
        return Err(Error::http(400, "千问办公设备授权请在系统浏览器中完成"));
    }
    let mut owner = params["payload"].clone();
    if let Some(fields) = owner.as_object_mut() {
        fields.remove("mode");
    }
    credentials::owner(&owner)?;
    let flow = flow(params)?;
    let before = state::get(Scope::Login, KEY, 1)?;
    if !before.value.is_null() {
        return Err(changed());
    }
    let verifier = util::random_secret()?;
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let nonce = util::uuid()?;
    let machine = machine::choose("")?;
    let now = util::now()?;
    let mut url = url::Url::parse(&client::url(endpoints::DEVICE_LOGIN)?)
        .map_err(|_| Error::new("千问办公授权地址配置无效"))?;
    url.query_pairs_mut()
        .append_pair("challenge", &challenge)
        .append_pair("challenge_method", "S256")
        .append_pair("machine_id", &machine)
        .append_pair("nonce", &nonce)
        .append_pair("client_id", endpoints::CLIENT_ID)
        .append_pair("redirect_uri", endpoints::REDIRECT_URI);
    put(
        &before,
        json!({ "flow": flow, "nonce": nonce, "verifier": verifier, "challenge": challenge,
        "machineId": machine, "locale": locale,
        "phase": "waiting", "expiresAt": now + TTL, "nextAt": now, "generation": util::uuid()? }),
        TTL,
    )?;
    Ok(
        json!({ "flow": flow, "url": url.to_string(), "poll_interval_ms": 2000,
        "instructions": login_methods::instructions(locale),
        "external_browser": true, "allowed_hosts": ["gateway.qwenwork.cn", "qwenwork.cn"],
        "login_ui": { "browserMode": "external", "manualCallback": false, "callbackInput": "url",
            "cookieCapture": null, "captureCallback": false } }),
    )
}
pub fn poll(params: &Value) -> Result<Value> {
    let flow = flow(params)?;
    let before = state::get(Scope::Login, KEY, 1)?;
    let now = util::now()?;
    let locale = before.value["locale"].as_str().unwrap_or("zh-CN");
    if !before.value.is_null() && before.value["flow"] != flow {
        return Err(changed());
    }
    if !before.compatible
        || before.value.is_null()
        || before.value["expiresAt"].as_i64().unwrap_or(0) <= now
    {
        put(&before, Value::Null, 1000)?;
        return Ok(failed(login_methods::progress(locale, "expired")));
    }
    if before.value["phase"] == "polling" {
        if before.value["busyUntil"].as_i64().unwrap_or(0) <= now {
            put(&before, Value::Null, 1000)?;
            return Ok(failed(login_methods::progress(locale, "unknown")));
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
    let current = state::get(Scope::Login, KEY, 1)?;
    if current.revision != reserved.revision
        || current.value["marker"] != marker
        || current.value["flow"] != flow
    {
        return Err(changed());
    }
    if working["expiresAt"].as_i64().unwrap_or(0) <= util::now()? {
        put(&reserved, Value::Null, 1000)?;
        return Ok(failed(login_methods::progress(locale, "expired")));
    }
    match result {
        Ok(Some(credentials)) => {
            let result = account::normalized(credentials, "web", "");
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
        Err(error) if error.code.as_deref() == Some("qwenwork_poll_retry") => {
            working["phase"] = json!("waiting");
            working["nextAt"] = json!(util::now()? + 5000);
            working["busyUntil"] = json!(0);
            put(&reserved, working, ttl)?;
            Ok(json!({ "state": "pending", "hint": login_methods::progress(locale, "retry") }))
        }
        Err(error) if error.code.as_deref() == Some("qwenwork_transport") => {
            // 断连时不确定取票是否已消费，保留本轮预约，不再次请求同一nonce。
            Ok(json!({ "state": "pending", "hint": login_methods::progress(locale, "unknown") }))
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
    let mut url = url::Url::parse(&client::url(endpoints::DEVICE_POLL)?)
        .map_err(|_| Error::new("千问办公轮询地址配置无效"))?;
    let nonce = value["nonce"].as_str().ok_or_else(changed)?;
    let verifier = value["verifier"].as_str().ok_or_else(changed)?;
    let machine = value["machineId"].as_str().ok_or_else(changed)?;
    url.query_pairs_mut()
        .append_pair("nonce", nonce)
        .append_pair("verifier", verifier)
        .append_pair("challenge_method", "S256");
    let path = format!("{}?{}", url.path(), url.query().unwrap_or(""));
    let mut headers = client::account_headers(None);
    headers["X-Request-Id"] = json!(util::uuid()?);
    let reply = client::request("GET", &path, headers, None, "设备授权轮询")?;
    if matches!(reply.status, 202 | 404) {
        return Ok(None);
    }
    if reply.status == 429 || reply.status >= 500 {
        return Err(Error::http(reply.status, "千问办公设备授权服务暂时不可达")
            .with_code("qwenwork_poll_retry"));
    }
    let data = client::accepted(reply, "设备授权")?;
    let data = client::unwrap_data(&data);
    for (field, expected) in [("nonce", nonce), ("code_challenge_method", "S256")] {
        if data
            .get(field)
            .is_some_and(|value| value.as_str() != Some(expected))
        {
            return Err(
                Error::http(409, "千问办公设备授权绑定不一致").with_code("identity_changed")
            );
        }
    }
    if let Some(challenge) = value["challenge"].as_str() {
        if data
            .get("code_challenge")
            .is_some_and(|value| value.as_str() != Some(challenge))
        {
            return Err(
                Error::http(409, "千问办公设备授权校验码不一致").with_code("identity_changed")
            );
        }
    }
    let access = util::secret(data, &["token", "device_token", "deviceToken"])?;
    if access.is_empty() {
        return Ok(None);
    }
    let refresh = util::secret(data, &["refresh_token", "refreshToken"])?;
    if access.contains('|') || refresh.contains('|') {
        return Err(Error::http(502, "千问办公设备授权未返回有效凭据"));
    }
    let user = util::text(data, &["user_id", "uid", "userId"]);
    let expires =
        util::token_expiry(data, &access)?.unwrap_or(util::now()? + endpoints::DEVICE_LIFETIME);
    let mut credentials = Credentials::parse(
        &json!({ "provider": endpoints::PROVIDER, "mode": "cn", "accessToken": access, "refreshToken": refresh,
        "userId": user, "machineId": machine, "expiresAt": expires }),
    )?;
    if let Err(error) = auth::profile(&mut credentials) {
        if credentials.user.is_empty() || util::terminal(&error) {
            return Err(if error.code.as_deref() == Some("qwenwork_transport") {
                Error::http(502, "千问办公已返回设备凭据但未能确认用户身份，请重新授权")
            } else {
                error
            });
        }
    }
    let received_at = util::now()?;
    if credentials.expires.is_some_and(|at| at <= received_at) {
        return Err(Error::http(502, "千问办公设备授权返回了已过期凭据"));
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
