use agent2api_wasm_sdk::{self as sdk, http, json, AccountLock, Error, Result, Value};

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter().find_map(|key| value[*key].as_str().map(str::trim).filter(|text| !text.is_empty()))
        .unwrap_or("").to_string()
}

fn number(value: &Value, keys: &[&str]) -> i64 {
    keys.iter().find_map(|key| value[*key].as_i64().filter(|n| *n > 0)).unwrap_or(0)
}

pub fn expires(secret: &Value) -> i64 {
    let explicit = number(secret, &["expiresAt"]);
    if explicit > 0 { return explicit; }
    let created = number(secret, &["createdAt", "created_at"]);
    let lifetime = number(secret, &["expiresIn", "expires_in"]);
    if created > 0 && lifetime > 0 { created.saturating_add(lifetime).saturating_mul(1000) } else { 0 }
}

pub fn expiring(secret: &Value) -> bool {
    let expires = expires(secret);
    !text(secret, &["refreshToken", "refresh_token"]).is_empty()
        && expires > 0 && expires.saturating_sub(3_600_000) <= sdk::now()
}

fn access(secret: &Value) -> Result<String> {
    let token = text(secret, &["accessToken", "access_token", "token"]);
    if token.is_empty() { Err(Error::http(401, "AtomCode 账号缺少访问令牌，请重新登录")) } else { Ok(token) }
}

pub fn identity(secret: &Value) -> String {
    let id = text(secret, &["userId", "user_id", "uid", "username", "userName"]);
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
        "uid": text(secret, &["userId", "user_id", "uid"]), "tokenTail": tail,
        "hasSession": !access.is_empty(), "hasAccessToken": !access.is_empty(),
        "hasRefreshToken": !text(secret, &["refreshToken", "refresh_token"]).is_empty(),
        "canRefresh": !text(secret, &["refreshToken", "refresh_token"]).is_empty()
    });
    if expires(secret) > 0 { result["expiresAt"] = json!(expires(secret)); }
    for key in ["email", "avatarUrl"] {
        let value = text(secret, &[key]);
        if !value.is_empty() { result[if key == "avatarUrl" { "avatar" } else { key }] = json!(value); }
    }
    result
}

pub fn normalized(secret: Value) -> Value {
    json!({ "identity": identity(&secret), "public": view(&secret), "secret": secret, "desktop": false })
}

pub fn bundle(payload: &Value, previous: Option<&Value>, fresh: bool) -> Result<Value> {
    let token = text(payload, &["accessToken", "access_token", "token"]);
    if token.is_empty() { return Err(Error::http(502, "AtomCode 登录代理未返回访问令牌")); }
    let mut secret = previous.filter(|value| value.is_object()).cloned().unwrap_or_else(|| json!({}));
    secret["accessToken"] = json!(token);
    let refresh = text(payload, &["refreshToken", "refresh_token"]);
    if !refresh.is_empty() { secret["refreshToken"] = json!(refresh); }
    let lifetime = number(payload, &["expiresIn", "expires_in"]);
    let created = number(payload, &["createdAt", "created_at"]);
    let created = if created > 0 { created } else if fresh { sdk::now() / 1000 } else { 0 };
    if created > 0 { secret["createdAt"] = json!(created); }
    if lifetime > 0 {
        secret["expiresIn"] = json!(lifetime);
        if created > 0 { secret["expiresAt"] = json!(created.saturating_add(lifetime).saturating_mul(1000)); }
    } else if let Some(previous) = previous {
        if expires(previous) > 0 { secret["expiresAt"] = json!(expires(previous)); }
    }
    let explicit = number(payload, &["expiresAt"]);
    if explicit > 0 { secret["expiresAt"] = json!(explicit); }
    for (target, aliases, user_key) in [
        ("userId", &["userId", "user_id", "uid"][..], "id"),
        ("username", &["username", "userName"][..], "username"),
        ("nickname", &["nickname"][..], "name"),
        ("email", &["email"][..], "email"),
        ("avatarUrl", &["avatarUrl", "avatar_url"][..], "avatar_url"),
    ] {
        let mut value = text(payload, aliases);
        if value.is_empty() { value = text(&payload["user"], &[user_key]); }
        if value.is_empty() && user_key == "id" { value = payload["user"]["id"].as_i64().map(|id| id.to_string()).unwrap_or_default(); }
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
    let mut secret = bundle(&payload, None, false)?;
    let name = text(params, &["name"]);
    if !name.is_empty() { secret["name"] = json!(name); }
    Ok(normalized(secret))
}

pub fn ensure() -> Result<String> {
    let record = sdk::account()?;
    if expiring(&record["secret"]) { refresh() } else { access(&record["secret"]) }
}

pub fn refresh() -> Result<String> {
    let before = sdk::account()?;
    let _lock = AccountLock::acquire()?;
    let current = sdk::account()?;
    if current["secret"] != before["secret"] { return access(&current["secret"]); }
    // 并发请求可能刚好在前一轮落盘后到达；短期合并仅缓存已成功写回的结果。
    let key = format!("refresh:{}", text(&current, &["id"]));
    if let Ok(recent) = sdk::session_get(&key) {
        if recent["access"].as_str().is_some_and(|token| access(&current["secret"]).is_ok_and(|current| current == token)) {
            return access(&current["secret"]);
        }
    }
    let refresh = text(&current["secret"], &["refreshToken", "refresh_token"]);
    if refresh.is_empty() { return Err(Error::http(401, "AtomCode 没有续期令牌，请重新登录")); }
    let url = format!("{}/oauth/refresh", super::upstream::base("brokerBase"));
    let response = http::open("POST", &url, super::upstream::headers(None), Some(&json!({ "refresh_token": refresh })))?;
    if matches!(response.status, 401 | 403) { return Err(Error::http(401, "AtomGit 续期令牌已失效，请重新登录")); }
    let fresh = response.json()?;
    let next = bundle(&fresh, Some(&current["secret"]), true)?;
    let result = sdk::host("account.update", json!({ "expect": current["secret"], "secret": next, "public": view(&next) }))?;
    if result["updated"] != true { return access(&sdk::account()?["secret"]); }
    let token = access(&next)?;
    let _ = sdk::session_put(&key, json!({ "access": token }), 5000);
    Ok(token)
}
