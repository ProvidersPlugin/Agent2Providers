use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

use super::upstream;

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter().find_map(|key| value[*key].as_str().map(str::trim).filter(|text| !text.is_empty()))
        .unwrap_or("").to_string()
}

fn number(value: &Value, keys: &[&str]) -> i64 {
    keys.iter().find_map(|key| value[*key].as_i64()
        .or_else(|| value[*key].as_str().and_then(|raw| raw.parse().ok())).filter(|n| *n > 0)).unwrap_or(0)
}

fn created(value: &Value) -> i64 {
    let number = number(value, &["createdAt", "created_at"]);
    if number > 0 { return if number >= 1_000_000_000_000 { number / 1000 } else { number }; }
    chrono::DateTime::parse_from_rfc3339(&text(value, &["createdAt", "created_at"]))
        .ok().map(|time| time.timestamp()).unwrap_or(0)
}

pub fn user_id(value: &Value) -> String {
    ["userId", "user_id", "uid"].iter().find_map(|key| {
        let value = &value[*key];
        value.as_str().map(str::trim).filter(|text| !text.is_empty()).map(str::to_string)
            .or_else(|| value.as_u64().map(|id| id.to_string()))
    }).or_else(|| value["user"]["id"].as_str().map(str::trim)
        .filter(|text| !text.is_empty()).map(str::to_string)
        .or_else(|| value["user"]["id"].as_u64().map(|id| id.to_string())))
        .unwrap_or_default()
}

fn record_identity(record: &Value) -> String {
    let saved = text(record, &["identity"]);
    if saved.is_empty() { identity(&record["secret"]) } else { saved }
}

pub fn expires(secret: &Value) -> i64 {
    let explicit = number(secret, &["expiresAt"]);
    if explicit > 0 { return explicit; }
    let legacy = number(secret, &["expires_at"]);
    if legacy > 0 { return if legacy < 1_000_000_000_000 { legacy.saturating_mul(1000) } else { legacy }; }
    let created = created(secret);
    let lifetime = number(secret, &["expiresIn", "expires_in"]);
    if created > 0 && lifetime > 0 { created.saturating_add(lifetime).saturating_mul(1000) } else { 0 }
}

pub fn expiring_within(secret: &Value, min_lead_ms: i64) -> bool {
    let expires = expires(secret);
    let lead = 3_600_000_i64.max(min_lead_ms.max(0));
    !text(secret, &["refreshToken", "refresh_token"]).is_empty()
        && expires > 0 && expires.saturating_sub(lead) <= sdk::now()
}

pub fn expiring(secret: &Value) -> bool {
    expiring_within(secret, 0)
}

pub fn access(secret: &Value) -> Result<String> {
    let token = text(secret, &["accessToken", "access_token", "token"]);
    if token.is_empty() { Err(Error::http(401, "AtomCode 账号缺少访问令牌，请重新登录")) } else { Ok(token) }
}

pub fn identity(secret: &Value) -> String {
    let saved = text(secret, &["accountIdentity"]);
    if !saved.is_empty() { return saved; }
    let id = user_id(secret);
    let id = if id.is_empty() { text(secret, &["username", "userName"]) } else { id };
    if id.is_empty() {
        use sha2::{Digest, Sha256};
        let credential = text(secret, &["refreshToken", "refresh_token", "accessToken", "access_token", "token"]);
        if credential.is_empty() { String::new() } else { format!("credential-{:x}", Sha256::digest(credential.as_bytes())) }
    } else { format!("user-{id}") }
}

pub fn view(secret: &Value) -> Value {
    let name = text(secret, &["name", "username", "userName", "nickname"]);
    let access = text(secret, &["accessToken", "access_token", "token"]);
    let tail: String = access.chars().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect();
    let mut result = json!({
        "name": if name.is_empty() { "AtomCode 账号" } else { &name },
        "uid": user_id(secret), "tokenTail": tail,
        "hasSession": !access.is_empty(), "hasAccessToken": !access.is_empty(),
        "hasRefreshToken": !text(secret, &["refreshToken", "refresh_token"]).is_empty(),
        "canRefresh": !text(secret, &["refreshToken", "refresh_token"]).is_empty()
    });
    if expires(secret) > 0 { result["expiresAt"] = json!(expires(secret)); }
    for (target, aliases) in [("email", &["email"][..]), ("avatar", &["avatarUrl", "avatar_url"][..])] {
        let value = text(secret, aliases);
        if !value.is_empty() { result[target] = json!(value); }
    }
    result
}

pub fn normalized(mut secret: Value) -> Value {
    let identity = identity(&secret);
    secret["accountIdentity"] = json!(identity);
    json!({ "identity": identity, "public": view(&secret), "secret": secret, "desktop": false })
}

pub fn bundle(payload: &Value, previous: Option<&Value>, fresh: bool) -> Result<Value> {
    if !payload.is_object() { return Err(Error::http(400, "AtomCode 凭据必须是 JSON 字段对象")); }
    let token = text(payload, &["accessToken", "access_token", "token"]);
    if token.is_empty() { return Err(Error::http(502, "AtomCode 登录代理未返回访问令牌")); }
    let mut secret = previous.filter(|value| value.is_object()).cloned().unwrap_or_else(|| json!({}));
    secret["accessToken"] = json!(token);
    let refresh = text(payload, &["refreshToken", "refresh_token"]);
    if !refresh.is_empty() { secret["refreshToken"] = json!(refresh); }
    let supplied_lifetime = number(payload, &["expiresIn", "expires_in"]);
    let lifetime = if supplied_lifetime > 0 { supplied_lifetime }
        else if fresh { previous.map(|value| number(value, &["expiresIn", "expires_in"])).unwrap_or(0) } else { 0 };
    let created = created(payload);
    let created = if created > 0 { created } else if fresh { sdk::now() / 1000 } else { 0 };
    if created > 0 { secret["createdAt"] = json!(created); }
    if lifetime > 0 {
        secret["expiresIn"] = json!(lifetime);
        if created > 0 { secret["expiresAt"] = json!(created.saturating_add(lifetime).saturating_mul(1000)); }
    }
    // 换到新令牌但未知新期限时不沿用已过期的 expiresAt，避免每次调用又续期。
    if fresh && lifetime == 0 {
        if let Some(fields) = secret.as_object_mut() {
            for key in ["expiresAt", "expires_at", "expiresIn", "expires_in"] { fields.remove(key); }
        }
    }
    let explicit = number(payload, &["expiresAt"]);
    let legacy = number(payload, &["expires_at"]);
    let explicit = if explicit > 0 { explicit }
        else if legacy > 0 && legacy < 1_000_000_000_000 { legacy.saturating_mul(1000) } else { legacy };
    if explicit > 0 { secret["expiresAt"] = json!(explicit); }
    let mut id = user_id(payload);
    if id.is_empty() { id = text(&payload["user"], &["id"]); }
    if id.is_empty() { id = payload["user"]["id"].as_u64().map(|id| id.to_string()).unwrap_or_default(); }
    let old_id = previous.map(user_id).unwrap_or_default();
    if !id.is_empty() && !old_id.is_empty() && id != old_id {
        return Err(Error::http(401, "AtomCode 续期返回了不同的账号身份，未覆盖原账号"));
    }
    if !id.is_empty() { secret["userId"] = json!(id); }
    for (target, aliases, user_keys) in [
        ("username", &["username", "userName"][..], &["username", "login"][..]),
        ("nickname", &["nickname"][..], &["name"][..]),
        ("email", &["email"][..], &["email"][..]),
        ("avatarUrl", &["avatarUrl", "avatar_url"][..], &["avatar_url"][..]),
    ] {
        let mut value = text(payload, aliases);
        if value.is_empty() { value = text(&payload["user"], user_keys); }
        if !value.is_empty() { secret[target] = json!(value); }
    }
    let name = text(payload, &["name"]);
    if !name.is_empty() { secret["name"] = json!(name); }
    if text(&secret, &["name"]).is_empty() {
        let name = text(&secret, &["username", "nickname", "userId"]);
        secret["name"] = json!(if name.is_empty() { "AtomCode 账号" } else { &name });
    }
    Ok(secret)
}

pub fn fill_profile(secret: &mut Value) -> Result<()> {
    if !user_id(secret).is_empty() { return Ok(()); }
    // 官方只读用户资料接口；不消耗 refresh，也不借客户端本机文件补签名身份。
    let profile = upstream::get("/user", &access(secret)?)?;
    let id = text(&profile, &["id"]);
    let id = if id.is_empty() { profile["id"].as_u64().map(|id| id.to_string()).unwrap_or_default() } else { id };
    if id.is_empty() || id.len() > 256 || id.contains('\0') {
        return Err(Error::http(502, "AtomCode 未取得请求签名所需的用户身份，请使用网页登录或补充用户 ID"));
    }
    secret["userId"] = json!(id);
    for (target, aliases) in [("username", &["username", "login"][..]), ("nickname", &["name"][..]),
        ("email", &["email"][..]), ("avatarUrl", &["avatar_url"][..])] {
        let value = text(&profile, aliases);
        if text(secret, &[target]).is_empty() && !value.is_empty() { secret[target] = json!(value); }
    }
    Ok(())
}

pub fn normalize(params: &Value) -> Result<Value> {
    let source = &params["source"];
    let payload = match source["kind"].as_str() {
        Some("manual") => {
            let payload = &source["payload"];
            if payload["inputs"].is_object() { payload["inputs"].clone() } else { payload.clone() }
        }
        Some("file_text") => serde_json::from_str(source["content"].as_str().unwrap_or(""))
            .map_err(|_| Error::http(400, "凭证文件不是合法 JSON"))?,
        _ => return Err(Error::http(400, "AtomCode 请使用网页登录或手动填写令牌")),
    };
    let payload = if let Some(raw) = payload.as_str() { json!({ "accessToken": raw }) } else { payload };
    let payload = if payload["credentialJson"].as_str().is_some_and(|text| !text.trim().is_empty()) {
        serde_json::from_str(payload["credentialJson"].as_str().unwrap_or(""))
            .map_err(|_| Error::http(400, "凭证 JSON 格式不正确"))?
    } else { payload };
    let payload = if let Some(raw) = payload.as_str() { json!({ "accessToken": raw }) } else { payload };
    let mut secret = bundle(&payload, None, false)?;
    // 身份读取暂不可达不丢掉用户原来可导入的凭据；真正对话前会再补全。
    if user_id(&secret).is_empty() { let _ = fill_profile(&mut secret); }
    let name = text(params, &["name"]);
    if !name.is_empty() { secret["name"] = json!(name); }
    Ok(normalized(secret))
}

pub fn ensure() -> Result<String> {
    let record = sdk::account()?;
    if expiring(&record["secret"]) { refresh() } else { access(&record["secret"]) }
}

pub fn signing_session() -> Result<String> {
    ensure()?;
    let record = sdk::account()?;
    if !user_id(&record["secret"]).is_empty() { return access(&record["secret"]); }
    let _lock = AccountLock::acquire()?;
    let current = sdk::account()?;
    if !user_id(&current["secret"]).is_empty() { return access(&current["secret"]); }
    let mut next = current["secret"].clone();
    next["accountIdentity"] = json!(record_identity(&current));
    fill_profile(&mut next)?;
    let result = sdk::host("account.update", json!({ "expect": current["secret"], "secret": next, "public": view(&next) }))?;
    if result["updated"] != true {
        let current = sdk::account()?;
        if user_id(&current["secret"]).is_empty() { return Err(Error::http(409, "AtomCode 账号在补全身份期间已变化，请重试")); }
        return access(&current["secret"]);
    }
    access(&next)
}

pub fn refresh() -> Result<String> {
    let before = sdk::account()?;
    let _lock = AccountLock::acquire()?;
    let current = sdk::account()?;
    if current["secret"] != before["secret"] { return access(&current["secret"]); }
    let key = format!("refresh:{}", text(&current, &["id"]));
    if let Ok(recent) = sdk::session_get(&key) {
        if recent["access"].as_str().is_some_and(|token| access(&current["secret"]).is_ok_and(|current| current == token)) {
            return access(&current["secret"]);
        }
    }
    let refresh = text(&current["secret"], &["refreshToken", "refresh_token"]);
    if refresh.is_empty() { return Err(Error::http(401, "AtomCode 没有续期令牌，请重新登录")); }
    let url = format!("{}/oauth/refresh", upstream::base("brokerBase"));
    let response = sdk::http::open_timeout("POST", &url, upstream::headers(None),
        Some(&json!({ "refresh_token": refresh })), 30_000)?;
    if matches!(response.status, 400 | 401 | 403) {
        return Err(Error::http(401, "AtomGit 续期令牌已失效，请重新登录").with_code("atomcode_refresh_expired"));
    }
    let fresh = upstream::response_json(response)?;
    let mut next = bundle(&fresh, Some(&current["secret"]), true)?;
    next["accountIdentity"] = json!(record_identity(&current));
    let result = sdk::host("account.update", json!({ "expect": current["secret"], "secret": next, "public": view(&next) }))?;
    if result["updated"] != true { return access(&sdk::account()?["secret"]); }
    let token = access(&next)?;
    let _ = sdk::session_put(&key, json!({ "access": token }), 5000);
    Ok(token)
}
