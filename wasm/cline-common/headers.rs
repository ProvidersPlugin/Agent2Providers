//! 客户端头覆盖仍按本池settings读取；固定鉴权/任务/传输头不能被用户覆盖。

use super::endpoints;
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use std::collections::BTreeMap;

const DEFAULTS: &[(&str, &str)] = &[
    ("User-Agent", "Cline/3.0.62"),
    ("HTTP-Referer", "https://cline.bot"),
    ("X-Title", "Cline"),
    ("X-IS-MULTIROOT", "false"),
    ("X-CLIENT-TYPE", endpoints::CLIENT_TYPE),
    ("X-CLIENT-VERSION", "3.0.62"),
    ("X-PLATFORM", "terminal"),
    ("X-PLATFORM-VERSION", "3.0.62"),
    ("X-CORE-VERSION", "0.0.70"),
];
const FIXED: &[&str] = &[
    "authorization",
    "content-type",
    "accept",
    "x-task-id",
    "host",
    "connection",
    "content-length",
    "transfer-encoding",
    "proxy-authorization",
];

fn table() -> Result<BTreeMap<String, String>> {
    let value = sdk::settings::get("upstreamHeaders")?;
    if value.is_null() {
        return Ok(BTreeMap::new());
    }
    let map = value
        .as_object()
        .filter(|map| map.len() <= 64)
        .ok_or_else(|| Error::http(400, "Cline 客户端头覆盖配置格式异常"))?;
    let mut out = BTreeMap::new();
    let mut seen = std::collections::HashSet::new();
    for (key, value) in map {
        let key = key.trim();
        let lower = key.to_ascii_lowercase();
        let value = value
            .as_str()
            .ok_or_else(|| Error::http(400, "Cline 请求头覆盖值必须是文本"))?;
        if key.is_empty()
            || key.len() > 128
            || !key
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&ch))
            || value.len() > 4096
            || value.bytes().any(|ch| ch == 127 || ch < 32 && ch != b'\t')
            || FIXED.contains(&lower.as_str())
            || !seen.insert(lower)
        {
            return Err(Error::http(400, "Cline 请求头覆盖包含非法项或固定头"));
        }
        out.insert(key.to_string(), value.to_string());
    }
    Ok(out)
}

fn merged(overrides: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut headers: BTreeMap<String, String> = DEFAULTS
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    for (key, value) in overrides {
        let effective_key = DEFAULTS
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(name, _)| *name)
            .unwrap_or(key);
        if value.is_empty() {
            headers.remove(effective_key);
        } else {
            headers.insert(effective_key.to_string(), value.clone());
        }
    }
    headers
}

pub fn chat(token: &str, session: &str) -> Result<Value> {
    let mut headers = merged(&table()?);
    headers.insert("content-type".into(), "application/json".into());
    headers.insert("accept".into(), "text/event-stream".into());
    headers.insert("authorization".into(), format!("Bearer {token}"));
    headers.insert("x-task-id".into(), session.into());
    Ok(json!(headers))
}

pub fn preview() -> Result<Value> {
    let overrides = table()?;
    let defaults: BTreeMap<&str, &str> = DEFAULTS.iter().copied().collect();
    Ok(json!({ "defaults": defaults, "overrides": overrides, "effective": merged(&overrides) }))
}
