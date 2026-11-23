//! 回调严格绑定宿主可信 origin、端口与原生 /oauth-callback；不接受凭据或裸 code 旁路。

use super::endpoints;
use agent2api_wasm_sdk::{Error, Result};
use std::collections::BTreeMap;
use url::{Host, Url};

fn invalid() -> Error {
    Error::http(400, "Antigravity 回调地址或 state 与本次登录不符，已拒绝")
}

fn receiver(raw: &str) -> Result<Url> {
    if raw.len() > 16_384 || raw.chars().any(char::is_control) || raw.contains('\\') {
        return Err(invalid());
    }
    let url = Url::parse(raw).map_err(|_| invalid())?;
    let loopback = match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        Some(Host::Domain("localhost")) => true,
        _ => false,
    };
    if url.scheme() != "http"
        || !loopback
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port_or_known_default().is_none_or(|p| p == 0)
    {
        return Err(invalid());
    }
    Ok(url)
}

pub fn redirect(trusted: &str) -> Result<String> {
    let mut url = receiver(trusted)?;
    if url.query().is_some()
        || !matches!(
            url.path(),
            "/api/session/login/wasm-callback" | "/oauth-callback"
        )
    {
        return Err(invalid());
    }
    url.set_path(endpoints::CALLBACK_PATH);
    Ok(url.to_string())
}

pub struct Received {
    pub code: Option<String>,
    pub denied: bool,
}

pub fn parse(raw: &str, expected: &str, state: &str) -> Result<Received> {
    // 浏览器/public 路由已提供完整原始 URL；不从 payload 或 Host 猜回调 origin。
    let expected = receiver(expected)?;
    let received = receiver(raw)?;
    let raw_target = raw
        .split_once("://")
        .and_then(|(_, rest)| rest.find('/').map(|at| &rest[at..]));
    if raw_target.and_then(|target| target.split('?').next()) != Some(endpoints::CALLBACK_PATH) {
        return Err(invalid());
    }
    if received.origin() != expected.origin()
        || received.path() != expected.path()
        || received.path() != endpoints::CALLBACK_PATH
    {
        return Err(invalid());
    }
    let mut fields = BTreeMap::new();
    for (key, value) in received.query_pairs() {
        // state、code 等重复值即使相同也拒绝，不允许不同组件选首值/末值分叉。
        if fields
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return Err(invalid());
        }
    }
    if fields.get("state").map(String::as_str) != Some(state) {
        return Err(invalid());
    }
    for forbidden in [
        "refreshToken",
        "refresh_token",
        "accessToken",
        "access_token",
        "redirect_uri",
        "token",
    ] {
        if fields.contains_key(forbidden) {
            return Err(invalid());
        }
    }
    let error = fields.get("error").filter(|v| !v.is_empty());
    let code = fields.get("code").filter(|v| !v.is_empty());
    if error.is_some() && code.is_some() {
        return Err(invalid());
    }
    if error.is_some() {
        return Ok(Received {
            code: None,
            denied: true,
        });
    }
    let code = code
        .filter(|v| v.len() <= 8192 && !v.chars().any(char::is_control))
        .ok_or_else(invalid)?;
    Ok(Received {
        code: Some(code.clone()),
        denied: false,
    })
}
