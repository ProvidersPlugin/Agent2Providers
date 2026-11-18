//! Kiro/AWS PKCE注册与单次换码；state/redirect只来自冻结的宿主阶段回执。
//! 不自建监听、不走设备码、不重试换码；缺少真实有效期保留未知。

use crate::{client, credentials, endpoints, errors, token_json, util};
use agent2api_wasm_sdk::{http, json, Error, Result, Value};

fn post(url: &str, body: &Value) -> Result<Value> {
    let response = http::open_timeout("POST", url, client::ide_headers()?, Some(body), 25_000)
        .map_err(errors::transport)?;
    let status = response.status;
    let bytes = response
        .bytes(token_json::LIMIT)
        .map_err(errors::transport)?;
    let value = token_json::parse(&bytes)?;
    // OAuth错误可能包含code/secret/URL，任何状态都不回显原文，也不重试。
    if !(200..300).contains(&status) {
        return Err(Error::http(
            status.max(400),
            format!("Kiro授权服务拒绝请求（HTTP {status}），请重新发起"),
        ));
    }
    if value.get("error").is_some_and(|value| !value.is_null()) {
        return Err(Error::http(502, "Kiro授权服务未返回有效结果"));
    }
    Ok(value)
}
pub fn portal(oauth_state: &str, verifier: &str, redirect: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("state", oauth_state)
        .append_pair("code_challenge", &util::challenge(verifier))
        .append_pair("code_challenge_method", "S256")
        .append_pair("redirect_uri", redirect)
        .append_pair("redirect_from", "KiroIDE")
        .finish();
    format!("{}/signin?{query}", endpoints::PORTAL)
}
pub fn start_aws(
    option: &str,
    issuer: &str,
    region: &str,
    oauth_state: &str,
    redirect: &str,
) -> Result<Value> {
    endpoints::auth_region(region)?;
    if !endpoints::https(issuer) {
        return Err(Error::http(400, "AWS组织登录地址须为有效HTTPS地址"));
    }
    let provider = match option {
        "builderid" => "builderid",
        "awsidc" => "enterprise",
        "internal" => "internal",
        _ => return Err(Error::http(400, "Kiro AWS授权方式无效")),
    };
    let verifier = util::nonce(48)?;
    // 保留原生Kiro login_oauth.rs的注册URI；浏览器/换码用宿主实际端口的完整redirect。
    // issuer只交原已授权地区的注册服务，不作为插件出站地址。
    let register = post(
        &format!("{}/client/register", endpoints::oidc(region)?),
        &json!({
        "clientName": "Kiro IDE", "clientType": "public", "scopes": endpoints::SCOPES,
        "grantTypes": ["authorization_code", "refresh_token"],
        "redirectUris": ["http://127.0.0.1/oauth/callback"], "issuerUrl": issuer }),
    )?;
    let client_id = credentials::oauth_token(&register, &["clientId"], true)?;
    let client_secret = credentials::oauth_token(&register, &["clientSecret"], true)?;
    if client_id.is_empty()
        || client_id.len() > 4096
        || client_id.chars().any(char::is_whitespace)
        || client_secret.is_empty()
        || client_secret.chars().any(char::is_whitespace)
    {
        return Err(Error::http(502, "AWS没有返回完整客户端注册信息"));
    }
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", redirect)
        .append_pair("scopes", &endpoints::SCOPES.join(","))
        .append_pair("state", oauth_state)
        .append_pair("code_challenge", &util::challenge(&verifier))
        .append_pair("code_challenge_method", "S256")
        .finish();
    Ok(
        json!({ "region": region, "client_id": client_id, "client_secret": client_secret,
        "provider": provider, "state": oauth_state, "verifier": verifier,
        "authorize_url": format!("{}/authorize?{query}", endpoints::oidc(region)?) }),
    )
}
fn verifier(value: &str) -> Result<()> {
    if !(43..=128).contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte))
    {
        return Err(Error::http(400, "Kiro授权材料无效，请重新发起"));
    }
    Ok(())
}
pub fn social(option: &str, code: &str, pkce: &str, redirect: &str) -> Result<Value> {
    verifier(pkce)?;
    if !matches!(option, "google" | "github")
        || !util::identifier(code, 4096)
        || code.chars().any(char::is_whitespace)
    {
        return Err(Error::http(400, "Kiro社交授权材料无效"));
    }
    let body = json!({ "code": code, "code_verifier": pkce, "redirect_uri": redirect });
    agent2api_wasm_sdk::host("login.kiro.exchange", json!({ "body": body.to_string() }))
}
pub fn aws(aws: &Value, code: &str, redirect: &str) -> Result<Value> {
    verifier(aws["verifier"].as_str().unwrap_or(""))?;
    if aws["client_id"].as_str().is_none_or(str::is_empty)
        || aws["client_secret"].as_str().is_none_or(str::is_empty)
        || !util::identifier(code, 4096)
        || code.chars().any(char::is_whitespace)
        || !matches!(
            aws["provider"].as_str(),
            Some("builderid" | "enterprise" | "internal")
        )
    {
        return Err(Error::http(400, "Kiro AWS注册材料已失效，请重新发起"));
    }
    let region = aws["region"].as_str().unwrap_or("");
    endpoints::auth_region(region)?;
    let body = json!({ "clientId": aws["client_id"], "clientSecret": aws["client_secret"],
        "grantType": "authorization_code", "redirectUri": redirect, "code": code, "codeVerifier": aws["verifier"] });
    agent2api_wasm_sdk::host("login.kiro.exchange", json!({ "body": body.to_string() }))
}
