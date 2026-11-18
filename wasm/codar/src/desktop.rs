//! 官方客户端文件只读且由宿主授权；持久账号只保存本机来源，不保存一次性令牌。
use agent2api_wasm_sdk::{self as sdk, json, Result, Value};

use crate::{account, credentials, endpoints, errors, util};

fn read() -> Result<Value> {
    sdk::native_call(endpoints::DESKTOP_HELPER, "read", json!({})).map_err(|mut error| {
        error.status = Some(503);
        error.message =
            "科搭本机登录态不可读，请先在官方客户端登录，或改用扫码、短信、令牌导入".into();
        error
    })
}

pub fn normalized(name: Option<&str>) -> Result<Value> {
    let local = read()?;
    let result = account::from_token(local["token"].as_str().unwrap_or(""), name, "desktop")?;
    let mut secret = result["secret"].clone();
    if let Some(fields) = secret.as_object_mut() {
        fields.remove("accessToken");
    }
    secret["desktopProfile"] = local["profile"].clone();
    secret["desktopReadOnly"] = json!(true);
    secret["desktopFingerprint"] = json!(util::digest(local["token"].as_str().unwrap_or("")));
    secret["expiresAt"] = json!(credentials::expires(local["token"].as_str().unwrap_or("")));
    account::normalized(secret)
}

pub fn access(secret: &Value) -> Result<String> {
    let local = read()?;
    let token = credentials::token(local["token"].as_str().unwrap_or(""))?;
    let claims = credentials::claims(&token)?;
    if claims["username"] != secret["username"]
        || secret["desktopProfile"]
            .as_str()
            .is_some_and(|profile| !profile.is_empty() && local["profile"] != profile)
    {
        return Err(errors::identity());
    }
    if credentials::expires(&token).is_some_and(|expires| expires <= sdk::now()) {
        return Err(agent2api_wasm_sdk::Error::http(
            401,
            "科搭客户端登录已过期，请先在官方客户端重新登录",
        ));
    }
    Ok(token)
}

pub fn sync(current: &account::Snapshot) -> Result<String> {
    let token = current.access()?;
    let fingerprint = util::digest(&token);
    if current.secret["desktopFingerprint"] == fingerprint {
        current.unchanged()?;
        return Ok(token);
    }
    current.verify_token(&token)?;
    if current.access()? != token {
        return Err(errors::changed());
    }
    let mut secret = current.secret.clone();
    secret["desktopFingerprint"] = json!(fingerprint);
    secret["desktopReadOnly"] = json!(true);
    secret["expiresAt"] = json!(credentials::expires(&token));
    current.unchanged()?;
    let updated = sdk::host(
        "account.update",
        json!({ "expect": current.secret, "secret": secret, "public": account::view(&secret)? }),
    )?;
    if updated["updated"] != true {
        return Err(errors::changed());
    }
    let latest = account::current()?;
    if latest.secret != secret || latest.access()? != token {
        return Err(errors::changed());
    }
    Ok(token)
}
