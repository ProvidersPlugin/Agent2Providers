//! 账号单飞锁 + 回读旧 secret + CAS。未落库的轮换令牌绝不用于后续请求。
//! API key 不刷新；本机引用无 helper 时拒绝，不拿账户里的历史 token 暗中顶替。
//! 外部 IdP 的 tokenURL 没有可信静态证据，保留凭证语义但不申请任意域。

use crate::{
    account, client,
    credentials::{self, Credentials},
    endpoints, errors, network, token_json, util,
};
use agent2api_wasm_sdk::{self as sdk, http, json, AccountLock, Error, Result, Value};

pub struct Session {
    pub secret: Value,
    pub credentials: Credentials,
}
fn read() -> Result<Session> {
    let value = sdk::account()?;
    let mut secret = value["secret"].clone();
    let credentials = Credentials::parse(&secret)?;
    let credentials = if credentials.local() {
        if value["desktop"] != true || value["provider"] != endpoints::ID {
            return Err(
                Error::http(403, "本机读取只适用于宿主确认的本机账号").with_code("forbidden")
            );
        }
        let mut current = crate::local::read()?;
        if credentials.source != "kiro" && current.source != credentials.source {
            return Err(Error::http(409, "Kiro 本机登录来源已变化，请重新导入"));
        }
        if !current.fresh(util::now()?, 0) {
            current.account_id = credentials.account_id;
            current.source = credentials.source;
            current.name = credentials.name;
            current.plan = credentials.plan;
            return Ok(Session { secret, credentials: current });
        }
        if secret["desktopFingerprint"] != util::hash(&current.access, 32)
            || current.method != credentials.method
            || current.profile != credentials.profile
        {
            let profile = crate::usage::fetch(&current)?;
            let email = crate::usage::verified_email(&profile, &current)?;
            if credentials.account_id.is_empty() {
                return Err(Error::http(
                    409,
                    "旧 Kiro 本机账号没有可核对的邮箱，请重新导入本机登录态",
                ));
            }
            if !email.eq_ignore_ascii_case(&credentials.account_id) {
                return Err(Error::http(
                    409,
                    "Kiro 客户端已切换账号，请重新导入本机登录态",
                ));
            }
            let mut reference = secret.clone();
            reference["desktopFingerprint"] = json!(util::hash(&current.access, 32));
            reference["expires"] = json!(current.expires);
            reference["profileArn"] = json!(current.profile);
            reference["method"] = json!(current.method);
            let mut public = account::view(&reference)?;
            public["needsRelogin"] = json!(false);
            public["needsReloginReason"] = json!("");
            if value["public"]["name"].is_string() { public["name"] = value["public"]["name"].clone(); }
            if sdk::host("account.update", json!({ "expect": secret, "secret": reference, "public": public }))?["updated"] != true {
                return Err(Error::http(409, "Kiro 本机账号在同步期间已变化，请重新发起"));
            }
            secret = reference;
        }
        current.account_id = credentials.account_id;
        // 旧通用来源仍维持原身份，读取路径由宿主绑定；不把引用落成令牌快照。
        current.source = credentials.source;
        current.name = credentials.name;
        current.plan = credentials.plan;
        current
    } else {
        credentials
    };
    Ok(Session {
        secret,
        credentials,
    })
}
fn save(session: &Session, credential: &Credentials, require_advance: bool) -> Result<Session> {
    let next = credential.merge(&session.secret);
    let saved = sdk::host(
        "account.update",
        json!({ "expect": session.secret,
        "secret": next, "public": account::view(&next)? }),
    )?;
    if saved["updated"] == true {
        return Ok(Session {
            secret: next,
            credentials: credential.clone(),
        });
    }
    let latest = read()?;
    if require_advance && !latest.credentials.advanced(&session.credentials) {
        return Err(
            Error::http(409, "Kiro 续期期间账号被修改，新凭据未保存，请重试")
                .with_code("kiro_account_changed"),
        );
    }
    Ok(latest)
}
// 两种续期仍沿原SDK/端点发一次；只替换响应读取，不经普通network::read丢重复字段。
fn token_response(url: &str, headers: Value, body: &Value, secrets: &[&str]) -> Result<Value> {
    let response =
        http::open_timeout("POST", url, headers, Some(body), 30_000).map_err(errors::transport)?;
    let status = response.status;
    let mime = network::header(&response.headers, "content-type").to_ascii_lowercase();
    if mime.contains("text/html") || mime.contains("application/xhtml+xml") {
        return Err(Error::http(502, "Kiro续期返回了网页而非凭据响应")
            .with_code("kiro_token_response_invalid"));
    }
    let bytes = response
        .bytes(token_json::LIMIT)
        .map_err(errors::transport)?;
    let value = token_json::parse(&bytes)?;
    if !(200..300).contains(&status) || value.get("error").is_some_and(|value| !value.is_null()) {
        return Err(errors::upstream(status, &value, secrets));
    }
    Ok(value)
}

fn local_expired() -> Error {
    Error::http(401, "Kiro 本机续期后仍未取得有效凭据，请在官方客户端重新登录")
        .with_code("kiro_auth")
}

fn renew(session: Session) -> Result<Session> {
    let c = &session.credentials;
    if c.local() {
        let status = sdk::native_call("desktop.refresh.kiro.v1", "status", json!({}))?;
        sdk::native_call("desktop.refresh.kiro.v1", "refresh", json!({ "sourceRevision":status["sourceRevision"] }))?;
        let current = read()?;
        if !current.credentials.fresh(util::now()?, 0) { return Err(local_expired()); }
        return Ok(current);
    }
    if c.api_key() {
        return Err(
            Error::http(401, "Kiro 拒绝了保存的 API key，请重新填写").with_code("kiro_auth")
        );
    }
    if c.refresh.is_empty() {
        return Err(Error::http(401, "Kiro 账号缺少刷新凭据，请重新登录").with_code("kiro_auth"));
    }
    let region = c.runtime_region()?;
    let reservation = sdk::refresh_guard::Reservation::acquire("credential-refresh", &c.refresh)?;
    let value =
        match c.method.as_str() {
            "social" => token_response(
                &endpoints::refresh(&region)?,
                client::social_headers()?,
                &json!({ "refreshToken": c.refresh }),
                &c.secrets(),
            )?,
            "idc" => {
                if c.client_id.is_empty() || c.client_secret.is_empty() {
                    return Err(Error::http(401, "Kiro AWS 客户端注册信息缺失，请重新授权")
                        .with_code("kiro_auth"));
                }
                // 注册客户端属于保存的认证目录地区；不能用profile业务区重写OIDC来源。
                token_response(
                    &format!("{}/token", endpoints::oidc(&c.region)?),
                    json!({ "Accept": "application/json", "Content-Type": "application/json" }),
                    &json!({ "clientId": c.client_id, "clientSecret": c.client_secret,
                    "refreshToken": c.refresh, "grantType": "refresh_token" }),
                    &c.secrets(),
                )?
            }
            "external-idp" => {
                let result = sdk::host("credential.kiro.external.refresh", json!({}))?;
                let mut next = c.clone();
                next.access = credentials::oauth_token(&result, &["accessToken"], true)?;
                next.refresh = credentials::oauth_token(&result, &["refreshToken"], true)?;
                next.expires = result["expiresAt"].as_i64();
                let saved = save(&session, &next, true)?;
                reservation.complete()?;
                return Ok(saved);
            }
            _ => return Err(Error::http(400, "Kiro 授权类型无效")),
        };
    let access = credentials::oauth_token(&value, &["accessToken", "access_token"], true)?;
    if access.starts_with("ksk_") {
        return Err(Error::http(
            502,
            "Kiro续期未返回真实OAuth访问令牌，旧凭据未覆盖",
        ));
    }
    let refresh = credentials::oauth_token(&value, &["refreshToken", "refresh_token"], false)?;
    let expires = credentials::oauth_expiry(&value)?;
    let mut next = c.clone();
    next.pin_identity();
    next.access = access;
    if !refresh.is_empty() {
        next.refresh = refresh;
    }
    // 新token未回期限就是未知，不能默认一小时或继承旧token的到期时刻。
    next.expires = expires;
    let saved = save(&session, &next, true)?;
    reservation.complete()?;
    Ok(saved)
}
pub fn lookup_profile(c: &Credentials) -> Result<String> {
    if c.api_key() {
        return Ok(String::new());
    }
    if c.login_provider == "builderid" {
        return Ok(endpoints::BUILDER_PROFILE.into());
    }
    let preferred = c.runtime_region()?;
    let mut tried = Vec::new();
    let mut last = Error::http(502, "Kiro 没有为此账号返回 profile");
    for region in [preferred.as_str(), "us-east-1", "eu-central-1"] {
        if tried.contains(&region) {
            continue;
        }
        tried.push(region);
        let url = format!(
            "{}/List-Available-Profiles?origin=KIRO_CLI",
            endpoints::management(region)?
        );
        match network::post(&url, network::auth(c), &json!({}), &c.secrets()) {
            Ok(value) => {
                if let Some(profile) = value["profiles"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|row| row["arn"].as_str())
                    .find(|arn| !arn.is_empty())
                {
                    if !util::identifier(profile, 2048) {
                        return Err(Error::http(502, "Kiro 返回的 profile 无效"));
                    }
                    endpoints::region_of(profile, &c.region)?;
                    return Ok(profile.into());
                }
            }
            Err(error) if errors::host_stop(&error) => return Err(error),
            Err(error)
                if c.method == "idc"
                    && error.status == Some(401)
                    && error.message.contains("Builder ID") =>
            {
                return Ok(endpoints::BUILDER_PROFILE.into());
            }
            Err(error) => last = error,
        }
    }
    Err(last)
}
fn profile(session: Session) -> Result<Session> {
    if session.credentials.local()
        || session.credentials.api_key()
        || !session.credentials.profile.is_empty()
    {
        return Ok(session);
    }
    match lookup_profile(&session.credentials) {
        Ok(profile) if !profile.is_empty() => {
            let mut next = session.credentials.clone();
            next.profile = profile;
            save(&session, &next, false)
        }
        Err(error) if errors::host_stop(&error) => Err(error),
        _ => Ok(session), // 原生首次 profile 为尽力补全；上游仍决定这份凭据是否能对话。
    }
}
pub fn ready() -> Result<Session> {
    let _lock = AccountLock::acquire()?;
    let current = read()?;
    let current = if current.credentials.local() {
        let status = sdk::native_call("desktop.refresh.kiro.v1", "status", json!({}))?;
        if !current.credentials.fresh(util::now()?, 0)
            || status["authorized"] == true && !current.credentials.fresh(util::now()?, endpoints::EARLY) {
            renew(current)?
        } else { current }
    } else if current.credentials.fresh(util::now()?, endpoints::EARLY)
        || !current.credentials.refreshable() && current.credentials.fresh(util::now()?, 0)
    {
        current
    } else {
        renew(current)?
    };
    profile(current)
}
pub fn after_rejection(rejected: &Credentials) -> Result<Session> {
    let _lock = AccountLock::acquire()?;
    let current = read()?;
    let current = if current.credentials.advanced(rejected) {
        current
    } else {
        renew(current)?
    };
    profile(current)
}
pub fn force() -> Result<Session> {
    after_rejection(&read()?.credentials)
}
pub fn expiring(min_lead_ms: i64) -> Result<bool> {
    // 本机续期只在独立授权有效且来源仍一致时参与后台维护。
    let lead = endpoints::LEAD.max(min_lead_ms.max(0));
    let account = sdk::account()?;
    let credentials = Credentials::parse(&account["secret"])?;
    if credentials.local() {
        let current = read()?;
        let status = sdk::native_call("desktop.refresh.kiro.v1", "status", json!({}))?;
        return Ok(status["authorized"] == true && status["refreshable"] == true
            && !current.credentials.fresh(util::now()?, lead));
    }
    Ok(credentials.refreshable()
        && sdk::refresh_guard::available("credential-refresh", &credentials.refresh)?
        && !credentials.fresh(util::now()?, lead))
}
