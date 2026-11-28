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
pub enum Callback { Code(String), Denied(String) }

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
        let error = pairs.get("error").map(String::as_str).unwrap_or("").trim();
        let description = pairs.get("error_description").map(String::as_str).unwrap_or("").trim();
        let detail = if description.is_empty() {
            error.to_string()
        } else {
            format!("{error}: {description}")
        };
        return Ok(Callback::Denied(detail));
    }
    Ok(Callback::Code(required(&pairs, "code", 4096)?.to_string()))
}
