//! 原生搜索保留/agenttool/v1/search独立通道，不使用聊天生成来源。
use crate::{headers, network, session, util};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

pub fn capabilities() -> Value {
    json!({ "version": 1, "search": true, "fetch": false, "max_query_chars": 4000 })
}
pub fn search(payload: &Value) -> Result<Value> {
    let started = sdk::now();
    let query = util::text(payload, &["query"]);
    if query.is_empty() || query.chars().count() > 4000 {
        return Err(Error::http(400, "搜索词长度需为1至4000字符"));
    }
    let timeout = payload["timeout_seconds"].as_u64().unwrap_or(30);
    if !(5..=600).contains(&timeout) {
        return Err(Error::http(400, "搜索超时需为5至600秒"));
    }
    let _ = session::ensure(false)?;
    let lock = AccountLock::acquire()?;
    let mut current = session::ensure_locked(&lock)?;
    let until = started.saturating_add((timeout as i64).saturating_mul(1000));
    // 单请求时限采用剩余用户预算，连接和读正文均由宿主同一截止时刻约束。
    for attempt in 0..2 {
        if sdk::now() >= until {
            return Err(Error::http(504, "WorkBuddy搜索等待已超时"));
        }
        let remaining = until.saturating_sub(sdk::now());
        if remaining < 1000 {
            return Err(Error::http(504, "WorkBuddy搜索等待已超时"));
        }
        let result = search_once(&current, &query, remaining as u64);
        if sdk::now() >= until {
            return Err(Error::http(504, "WorkBuddy搜索等待已超时"));
        }
        match result {
            Err(error) if error.status == Some(401) && attempt == 0 => {
                current = session::refresh_locked_after_rejection(&current, &lock)?;
            }
            result => return result,
        }
    }
    Err(Error::http(401, "WorkBuddy搜索认证失败"))
}
fn search_once(current: &session::Session, query: &str, timeout_ms: u64) -> Result<Value> {
    let mut headers = headers::chat(&current.secret)?;
    headers["Accept"] = json!("application/json");
    let reply = network::raw_timeout(
        "POST",
        &current.endpoint()?,
        "/agenttool/v1/search",
        headers,
        Some(&json!({ "query": query, "type": "text2text", "max_results": 5 })),
        timeout_ms,
    )?;
    if reply.code() == Some(15001) {
        let mut error = Error::http(429, "WorkBuddy搜索额度已用尽").with_code("rate_limited");
        error.upstream_code = Some(15001);
        return Err(error);
    }
    if !reply.ok() {
        return Err(crate::errors::upstream(
            reply.status,
            &reply.value,
            &current.secrets(),
        ));
    }
    let rows = reply.value["results"]
        .as_array()
        .ok_or_else(|| Error::http(502, "搜索响应缺少结果列表"))?;
    let sources: Vec<_> = rows.iter().take(50).filter_map(|row| {
        let raw = row["url"].as_str()?; let url = url::Url::parse(raw).ok()?;
        if !matches!(url.scheme(), "https" | "http") || url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() { return None; }
        let published = row["publishedDate"].as_str().map(str::to_string).or_else(|| row["publishedDate"].as_i64().and_then(chrono::DateTime::from_timestamp_millis).map(|v| v.to_rfc3339()));
        Some(json!({ "url": url.as_str(), "title": row["title"].as_str().unwrap_or(raw).chars().take(300).collect::<String>(),
            "content": row["snippet"].as_str().unwrap_or("").chars().take(2000).collect::<String>(), "publishedDate": published }))
    }).collect();
    session::check(current)?;
    Ok(json!({ "sources": sources }))
}
