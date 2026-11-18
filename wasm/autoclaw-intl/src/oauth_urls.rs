//! 国际 OAuth 的 URL 纯校验。此模块不打开浏览器、不监听端口、不读取回调文件。
//! 浏览器来源只接受通用宿主按 provider 审批的精确 HTTPS origins，不能来自表单。
//! 官方 state 与宿主 flow 分开保存；回调只认完整 localhost URL，不接受裸 code/query。
use std::collections::BTreeMap;
use agent2api_wasm_sdk::{Error, Result};
use url::Url;
use super::{endpoints, oauth::Vendor, util};

pub struct Authorization {
    pub url: String,
    pub official_state: String,
}
pub enum Callback { Code(String), Denied }

fn invalid() -> Error { Error::http(400, "AutoClaw 国际版回调地址、授权方式或参数无效") }
fn url(raw: &str, limit: usize) -> Result<Url> {
    if raw.is_empty() || !util::printable(raw, limit) || raw.bytes().any(|byte| byte.is_ascii_whitespace())
        || raw.contains('\\') {
        return Err(invalid());
    }
    let parsed = Url::parse(raw).map_err(|_| invalid())?;
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
        return Err(invalid());
    }
    Ok(parsed)
}

pub fn navigate_uri(vendor: Vendor, port: u16) -> Result<String> {
    if !endpoints::CALLBACK_PORTS.contains(&port) { return Err(invalid()); }
    Ok(format!("http://localhost:{port}/auth/callback-{}", vendor.id()))
}
pub fn navigate(raw: &str, vendor: Vendor) -> Result<Url> {
    let parsed = url(raw, 256)?;
    let port = parsed.port().ok_or_else(invalid)?;
    if parsed.scheme() != "http" || parsed.host_str() != Some("localhost") || parsed.query().is_some()
        || raw != navigate_uri(vendor, port)? {
        return Err(invalid());
    }
    Ok(parsed)
}

fn query(parsed: &Url) -> Result<BTreeMap<String, String>> {
    let raw = parsed.query().ok_or_else(invalid)?;
    let bytes = raw.as_bytes();
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes[offset] == b'%' {
            if offset + 2 >= bytes.len() || !bytes[offset + 1].is_ascii_hexdigit() || !bytes[offset + 2].is_ascii_hexdigit() {
                return Err(invalid());
            }
            offset += 3;
        } else { offset += 1; }
    }
    let mut pairs = BTreeMap::new();
    for (key, value) in parsed.query_pairs() {
        if pairs.len() >= 32 || key.is_empty() || !util::printable(&key, 128) || value.len() > 8192
            || value.chars().any(char::is_control) || value.contains('\u{fffd}')
            || pairs.insert(key.into_owned(), value.into_owned()).is_some() {
            return Err(invalid());
        }
    }
    Ok(pairs)
}
fn required<'a>(pairs: &'a BTreeMap<String, String>, key: &str, max: usize) -> Result<&'a str> {
    pairs.get(key).map(String::as_str).filter(|value| !value.is_empty() && util::printable(value, max)
        && !value.bytes().any(|byte| byte.is_ascii_whitespace())).ok_or_else(invalid)
}

/// 只处理固定国际接口返回的 oauth_url；approved_origins 是内部宿主授权，不是用户输入。
/// 保留 URL 原文，包括可能由上游代管的 PKCE challenge，不改 state/redirect_uri。
pub fn authorization(raw: &str, vendor: Vendor, navigate_uri: &str, approved_origins: &[String]) -> Result<Authorization> {
    navigate(navigate_uri, vendor)?;
    if approved_origins.is_empty() || approved_origins.len() > 8 {
        return Err(Error::http(503, "主程序尚未提供国际版官方授权页的受控来源许可"));
    }
    for origin in approved_origins {
        let parsed = url(origin, 512)?;
        if parsed.scheme() != "https" || parsed.port_or_known_default() != Some(443) || parsed.query().is_some()
            || parsed.path() != "/" || parsed.origin().ascii_serialization() != *origin {
            return Err(Error::coded("主程序授权页来源许可必须是精确的 HTTPS origin", "forbidden"));
        }
        match parsed.host() {
            Some(url::Host::Domain(host)) if host.contains('.') && !host.ends_with('.') && !host.contains('*') => {},
            _ => return Err(Error::coded("不允许把本机、IP 或通配来源作为国际版官方授权页", "forbidden")),
        }
    }
    let parsed = url(raw, 16 * 1024)?;
    if parsed.scheme() != "https" || !approved_origins.contains(&parsed.origin().ascii_serialization()) {
        return Err(Error::coded("AutoClaw 国际版授权页不在主程序批准的官方来源内", "forbidden"));
    }
    let pairs = query(&parsed)?;
    let official_state = required(&pairs, "state", 4096)?.to_string();
    if pairs.get("redirect_uri").is_none_or(|value| value != navigate_uri)
        || pairs.get("response_type").is_some_and(|value| value != "code") {
        return Err(Error::http(502, "AutoClaw 官方授权地址没有绑定本轮登记的回调"));
    }
    Ok(Authorization { url: raw.into(), official_state })
}

pub fn callback(raw: &str, vendor: Vendor, navigate_uri: &str, official_state: &str) -> Result<Callback> {
    navigate(navigate_uri, vendor)?;
    if official_state.is_empty() || !util::printable(official_state, 4096) { return Err(invalid()); }
    let parsed = url(raw, 8192)?;
    // 原字符串也必须逐字匹配，拒绝别名主机、点段归一化、编码路径和其它登记端口。
    let base = raw.split_once('?').map(|(base, _)| base).ok_or_else(invalid)?;
    if base != navigate_uri || parsed.scheme() != "http" || parsed.host_str() != Some("localhost")
        || parsed.path() != format!("/auth/callback-{}", vendor.id()) {
        return Err(invalid());
    }
    let pairs = query(&parsed)?;
    let state = required(&pairs, "state", 4096)?;
    if state != official_state {
        return Err(Error::http(400, "AutoClaw 官方回调不属于本轮授权，未发送授权码"));
    }
    if pairs.contains_key("error") {
        if pairs.contains_key("code") || required(&pairs, "error", 256).is_err() { return Err(invalid()); }
        return Ok(Callback::Denied);
    }
    Ok(Callback::Code(required(&pairs, "code", 4096)?.to_string()))
}
