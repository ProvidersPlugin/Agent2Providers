//! 原生已有的InvokeMCP独立网页搜索，不创建对话、不以模型回答捏造来源。
//! 只读当次真实来源URL，不下载搜索结果；凭据/URL鉴权不会进入公开结果。

use crate::{client, credentials::Credentials, endpoints, errors, network, refresh, util};
use agent2api_wasm_sdk::{http, json, Error, Result, Value};

pub fn capabilities() -> Value {
    json!({
        "version": 1,
        "search": true,
        "fetch": false,
        "max_query_chars": 200,
        "supports_max_results": false
    })
}
fn invalid() -> Error {
    Error::http(502, "Kiro搜索返回了无法识别的结果").with_code("provider_unavailable")
}
fn sources(value: &Value, c: &Credentials, limit: Option<usize>) -> Result<Value> {
    let result = value.get("result").ok_or_else(invalid)?;
    let result = if let Some(text) = result.as_str() {
        serde_json::from_str::<Value>(text).map_err(|_| invalid())?
    } else {
        result.clone()
    };
    if result["isError"] == true {
        return Err(invalid());
    }
    let text = result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| item["type"] == "text")
        .and_then(|item| item["text"].as_str())
        .ok_or_else(invalid)?;
    let payload: Value = serde_json::from_str(text).map_err(|_| invalid())?;
    if payload.get("error").is_some_and(|error| !error.is_null()) {
        return Err(invalid());
    }
    let items = payload["results"].as_array().ok_or_else(invalid)?;
    let rows: Vec<_> = items.iter().take(limit.unwrap_or(200)).filter_map(|item| {
        let raw = item["url"].as_str()?.trim();
        let url = url::Url::parse(raw).ok()?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() || !url.username().is_empty()
            || url.password().is_some() || errors::redact(raw, &c.secrets()) != raw { return None; }
        let title = util::bounded(item["title"].as_str().unwrap_or(raw).trim(), 1200);
        let content = util::bounded(item["snippet"].as_str().unwrap_or("").trim(), 8000);
        let date = if let Some(text) = item["publishedDate"].as_str() { Some(util::bounded(text, 100)) }
            else { item["publishedDate"].as_i64().filter(|ms| *ms > 0)
                .and_then(chrono::DateTime::from_timestamp_millis).map(|date| date.to_rfc3339()) };
        Some(json!({ "url": url.as_str(), "title": title, "content": content, "publishedDate": date }))
    }).collect();
    Ok(errors::sanitize(&json!({ "sources": rows }), &c.secrets()))
}
fn once(c: &Credentials, query: &str, timeout_ms: u64, limit: Option<usize>) -> Result<Value> {
    if !c.api_key() && c.profile.is_empty() {
        return Err(
            Error::http(400, "Kiro账号缺少搜索所需profile，请重新登录").with_code("configuration")
        );
    }
    let mut body = json!({ "jsonrpc": "2.0", "id": util::uuid()?, "method": "tools/call",
        "params": { "name": "web_search", "arguments": { "query": query } } });
    let mut headers = network::auth(c);
    headers["Content-Type"] = json!("application/x-amz-json-1.0");
    headers["x-amz-target"] = json!(endpoints::SEARCH_TARGET);
    headers["x-amzn-codewhisperer-optout"] = json!("true");
    headers["amz-sdk-invocation-id"] = json!(util::uuid()?);
    headers["amz-sdk-request"] = json!("attempt=1; max=1");
    let user_agent = client::streaming_agent()?;
    headers["x-amz-user-agent"] = json!(user_agent);
    headers["User-Agent"] = json!(user_agent);
    if !c.api_key() {
        body["profileArn"] = json!(c.profile);
        headers["x-amzn-kiro-profile-arn"] = json!(c.profile);
    }
    let response = http::open_timeout(
        "POST",
        &format!("{}/", endpoints::runtime(&c.runtime_region()?)?),
        headers,
        Some(&body),
        timeout_ms,
    )
    .map_err(errors::transport)?;
    let status = response.status;
    let bytes = response.bytes(2 * 1024 * 1024).map_err(errors::transport)?;
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) if !(200..300).contains(&status) => {
            return Err(errors::upstream(status, &Value::Null, &c.secrets()));
        }
        Err(_) => return Err(invalid()),
    };
    if status == 403 {
        let reason = value["reason"].as_str().unwrap_or("");
        let message = value["message"].as_str().unwrap_or("").to_ascii_lowercase();
        let expired = ["INVALID_TOKEN", "EXPIRED_TOKEN", "TOKEN_EXPIRED"].contains(&reason)
            || [
                "token is expired",
                "expired token",
                "invalid token",
                "invalid bearer token",
            ]
            .iter()
            .any(|needle| message.contains(needle));
        if !expired {
            return Err(Error::http(403, "Kiro搜索服务拒绝该账号，请检查权限")
                .with_code("provider_unavailable"));
        }
    }
    if !(200..300).contains(&status) || value.get("error").is_some_and(|value| !value.is_null()) {
        return Err(errors::upstream(status, &value, &c.secrets()));
    }
    sources(&value, c, limit)
}
pub fn search(payload: &Value) -> Result<Value> {
    let query = payload["query"].as_str().unwrap_or("").trim();
    if query.is_empty() || query.chars().count() > 200 {
        return Err(Error::http(400, "Kiro搜索词需为1至200个字符").with_code("invalid_input"));
    }
    let timeout = payload["timeout_seconds"].as_u64().unwrap_or(30);
    if !(5..=600).contains(&timeout) {
        return Err(Error::http(400, "搜索超时需为5至600秒").with_code("invalid_input"));
    }
    let limit = match payload.get("max_results") {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_u64() {
            Some(count) if count >= 1 => Some((count as usize).min(200)),
            _ => return Err(Error::http(400, "搜索结果条数需为正整数").with_code("invalid_input")),
        },
    };
    let deadline = util::now()?.saturating_add((timeout * 1000) as i64);
    let mut session = refresh::ready()?;
    for attempt in 0..2 {
        let left = deadline.saturating_sub(util::now()?);
        if left < 1000 {
            return Err(Error::http(504, "Kiro搜索请求超时").with_code("timed_out"));
        }
        match once(&session.credentials, query, left as u64, limit) {
            Err(error)
                if error.status == Some(401) && attempt == 0 && !session.credentials.api_key() =>
            {
                session = refresh::after_rejection(&session.credentials)?;
            }
            result => return result,
        }
    }
    Err(Error::http(401, "Kiro搜索账号登录态失效").with_code("invalid_provider_key"))
}
