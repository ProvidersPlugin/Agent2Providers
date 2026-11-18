use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

use crate::{account, credentials, desktop, endpoints, errors, network, util};

const BEFORE: i64 = 48 * 60 * 60 * 1000;

pub fn expiring(secret: &Value) -> bool {
    if secret["source"] == "desktop" {
        // 只读本机关联不消费客户端的续期材料；到实际过期才提示客户端重登。
        return secret["expiresAt"]
            .as_i64()
            .is_some_and(|at| at <= sdk::now());
    }
    let expires = account::access(secret)
        .ok()
        .and_then(|token| credentials::expires(&token));
    expires.is_some_and(|at| at.saturating_sub(sdk::now()) <= BEFORE)
}

pub fn ensure(force: bool) -> Result<String> {
    let before = account::current()?;
    if before.secret["source"] == "desktop" {
        let _lock = sdk::AccountLock::acquire()?;
        return desktop::sync(&account::current()?);
    }
    if !force && !expiring(&before.secret) {
        return before.access();
    }
    let _lock = sdk::AccountLock::acquire()?;
    let current = account::current()?;
    if current.secret != before.secret {
        return current.access();
    }
    if !force && !expiring(&current.secret) {
        return current.access();
    }
    let recent = current.secret["refreshedAt"].as_i64().unwrap_or(0);
    if recent > 0 && sdk::now().saturating_sub(recent) < 5000 {
        return current.access();
    }
    let old = current.access()?;
    let reply = network::checked(
        "POST",
        endpoints::ACCOUNT,
        endpoints::REFRESH,
        &old,
        None,
        12_000,
    )?;
    let candidate = util::text(network::data(&reply), &["token", "accessToken"]);
    let candidate = if candidate.is_empty() {
        util::text(&reply, &["token", "accessToken"])
    } else {
        candidate
    };
    let token =
        credentials::token(&candidate).map_err(|_| Error::http(502, "科搭续期没有返回有效令牌"))?;
    if credentials::expires(&token).is_some_and(|expires| expires <= sdk::now()) {
        return Err(Error::http(401, "科搭续期返回的令牌已经过期，请重新登录"));
    }
    let claim = credentials::claims(&token)?;
    if util::text(&claim, &["username"]) != current.secret["username"].as_str().unwrap_or("") {
        return Err(errors::identity());
    }
    current.verify_token(&token)?;
    current.unchanged()?;
    let mut secret = current.secret.clone();
    secret["accessToken"] = json!(token);
    secret["refreshedAt"] = json!(util::now()?);
    let result = sdk::host(
        "account.update",
        json!({ "expect": current.secret, "secret": secret, "public": account::view(&secret)? }),
    )?;
    if result["updated"] != true {
        return Err(errors::changed());
    }
    Ok(token)
}
