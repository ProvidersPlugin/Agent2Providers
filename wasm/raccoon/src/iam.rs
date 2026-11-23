//! IAM平台身份只取user_info.id，sub不可当旧数字uid；仅200901触发一次iam/confirm。

use super::{config, endpoints, errors, jwt, network};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn confirm(token: &str) -> Result<()> {
    let mut headers = config::page(false)?;
    headers["Accept"] = json!("application/json");
    headers["X-Raccoon-Language"] = json!("zh");
    let reply = network::raw(
        "POST",
        &format!("{}/iam/confirm", endpoints::AUTH),
        token,
        "",
        headers,
        Some(&json!({})),
    )?;
    network::data(reply, &[token], true)?;
    Ok(())
}

pub fn profile(token: &str) -> Result<Value> {
    jwt::validate_iam(token)?;
    let path = format!("{}/user_info", endpoints::AUTH);
    let first = network::raw("GET", &path, token, "", config::page(false)?, None)?;
    let reply = if errors::code(&first.value) == Some(200_901) {
        confirm(token)?;
        network::raw("GET", &path, token, "", config::page(false)?, None)?
    } else {
        first
    };
    let profile = network::data(reply, &[token], true)?;
    if !profile["id"]
        .as_str()
        .is_some_and(|uid| !uid.trim().is_empty() && uid.len() <= 256)
    {
        return Err(Error::http(502, "小浣熊账号资料缺少平台用户ID"));
    }
    Ok(profile)
}
