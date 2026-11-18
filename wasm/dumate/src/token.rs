use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

use crate::{account, credentials::Cookies, endpoints, errors, network, util};

const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;

pub fn expiring(secret: &Value) -> bool {
    util::text(secret, &["appToken"]).is_empty()
        || util::timestamp(&secret["appTokenExpiresAt"])
            .is_none_or(|at| at.saturating_sub(REFRESH_SKEW_MS) <= sdk::now())
}

pub fn exchange(secret: &mut Value) -> Result<String> {
    let cookie = Cookies::parse(secret)?;
    let now = util::now()?;
    let reply = network::web(
        "GET",
        endpoints::TOKEN,
        &cookie,
        None,
        &cookie.protected(secret),
    )?;
    let token = util::text(&reply, &["token"]);
    if token.len() < 8
        || token.len() > 16 * 1024
        || token
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(Error::http(502, "百度搭子换证接口没有返回有效的模型令牌"));
    }
    let expires = match reply
        .get("expired_at")
        .filter(|value| !value.is_null() && value.as_str() != Some(""))
    {
        Some(value) => util::timestamp(value)
            .ok_or_else(|| Error::http(502, "百度搭子模型令牌到期时间格式无效"))?,
        None => now + 50 * 60 * 1000,
    };
    if expires <= now {
        return Err(Error::http(
            401,
            "百度搭子换证接口返回的令牌已过期，请重新登录",
        ));
    }
    secret["appToken"] = json!(token);
    secret["appTokenExpiresAt"] = json!(expires);
    secret["appTokenUpdatedAt"] = json!(now);
    Ok(token)
}

pub fn ensure(force: bool) -> Result<String> {
    let before = account::current()?;
    before.unchanged()?;
    if !force && !expiring(&before.secret) {
        return Ok(util::text(&before.secret, &["appToken"]));
    }
    let _lock = AccountLock::acquire()?;
    let current = account::current()?;
    if current.identity != before.identity
        || current.cookie.header != before.cookie.header
        || current.cookie.origin != before.cookie.origin
    {
        return Err(errors::changed());
    }
    if current.secret != before.secret && !expiring(&current.secret)
        || !force && !expiring(&current.secret)
    {
        current.unchanged()?;
        return Ok(util::text(&current.secret, &["appToken"]));
    }
    current.verify()?;
    let mut next = current.secret.clone();
    let token = exchange(&mut next)?;
    current.unchanged()?;
    let result = sdk::host(
        "account.update",
        json!({ "expect": current.secret, "secret": next, "public": account::view(&next)? }),
    )?;
    if result["updated"] != true {
        return Err(errors::changed());
    }
    let after = account::current()?;
    if after.identity != current.identity || after.secret != next {
        return Err(errors::changed());
    }
    after.unchanged()?;
    Ok(token)
}
