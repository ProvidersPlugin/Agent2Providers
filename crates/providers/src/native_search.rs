//! 账号型搜索的可选管理动作：只传查询与结果，不把凭证带回网关。
//! `web-search.capabilities` 不读账号、不出网；旧插件不认识动作时由网关提示更新。

use std::time::Duration;

use agent2api_provider_sdk::{PluginCtx, StoreProxy};
use futures::StreamExt;
use serde_json::{json, Value};

use crate::errors::GatewayError;

pub const CAPABILITIES: &str = "web-search.capabilities";
pub const SEARCH: &str = "web-search.search";
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

pub fn capabilities(max_query_chars: usize) -> Value {
    json!({ "version": 1, "search": true, "fetch": false, "max_query_chars": max_query_chars })
}

pub fn input(payload: &Value, max_query_chars: usize) -> Result<(String, Duration), GatewayError> {
    let query = payload.get("query").and_then(Value::as_str).unwrap_or("").trim();
    if query.is_empty() || query.chars().count() > max_query_chars {
        return Err(GatewayError::with_status(400, format!("搜索词需为 1～{max_query_chars} 个字符"))
            .with_code("invalid_input"));
    }
    let timeout = payload.get("timeout_seconds").and_then(Value::as_u64).unwrap_or(30);
    if !(5..=600).contains(&timeout) {
        return Err(GatewayError::with_status(400, "搜索超时需为 5～600 秒").with_code("invalid_input"));
    }
    Ok((query.to_string(), Duration::from_secs(timeout)))
}

/// 为认证、续期和搜索复用同一个账号出口；显式直连不能被全局代理替换。
pub async fn account_context(ctx: &PluginCtx, provider: &str, account: &str) -> Result<PluginCtx, String> {
    if account.is_empty() {
        return Err(GatewayError::with_status(400, "请先选择搜索使用的账号")
            .with_code("configuration").into_control_message());
    }
    let stored = ctx.store_get(provider, account).await.map_err(|_| {
        GatewayError::with_status(400, "搜索账号不存在或不属于该提供商")
            .with_code("configuration").into_control_message()
    })?;
    let mut next = ctx.clone();
    match stored.proxy {
        Some(StoreProxy::Resolved { proxy }) => next.env.proxy = proxy.to_url(),
        Some(StoreProxy::Static { config }) if config.get("source").and_then(Value::as_str) == Some("direct") => {
            next.env.proxy = "direct".to_string();
        }
        Some(StoreProxy::Static { config }) => match crate::proxy_util::resolve_account_proxy(Some(&config)) {
            Some(crate::proxy_util::ProxyResolution::Resolved(proxy)) => next.env.proxy = proxy.to_url(),
            Some(crate::proxy_util::ProxyResolution::Failed(_)) => return Err(proxy_error()),
            None => {}
        },
        Some(StoreProxy::Failed { .. }) => return Err(proxy_error()),
        Some(StoreProxy::Unset) | None => {}
    }
    Ok(next)
}

fn proxy_error() -> String {
    GatewayError::with_status(502, "搜索账号的出网代理不可用，请检查账号设置")
        .with_code("provider_unavailable").into_control_message()
}

pub fn timed_out() -> GatewayError {
    GatewayError::with_status(504, "搜索请求超时，请稍后重试").with_code("timed_out")
}

pub fn transport(error: reqwest::Error) -> GatewayError {
    if error.is_timeout() { timed_out() } else {
        // 不透传 reqwest 错误链，避免 URL、代理口令或凭证进入返回值。
        GatewayError::with_status(502, "无法连接搜索提供商").with_code("provider_unavailable")
    }
}

pub fn invalid_response() -> GatewayError {
    GatewayError::with_status(502, "搜索提供商返回了无法识别的结果")
        .with_code("provider_unavailable")
}

pub fn rejected(status: u16) -> GatewayError {
    let (code, message) = match status {
        401 => ("invalid_provider_key", "搜索账号登录态已失效，请重新登录"),
        403 => ("provider_unavailable", "搜索服务拒绝了该账号，请检查账号权限"),
        402 | 429 => ("rate_limited", "搜索额度已用完或触发限流，请稍后重试"),
        _ => ("provider_unavailable", "搜索提供商暂不可用"),
    };
    GatewayError::with_status(if status == 402 { 429 } else { i32::from(status.max(400)) }, message)
        .with_code(code)
}

pub async fn read_response(response: reqwest::Response) -> Result<(u16, Value), GatewayError> {
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport)?;
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(invalid_response());
        }
        bytes.extend_from_slice(&chunk);
    }
    let value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) if !(200..300).contains(&status) => return Err(rejected(status)),
        Err(_) => return Err(invalid_response()),
    };
    Ok((status, value))
}

/// 只收上游结果列表中的网页；不拿模型回答填充摘要、不合成不存在的来源。
pub fn sources(items: &[Value]) -> Value {
    let rows: Vec<Value> = items.iter().take(50).filter_map(|item| {
        let raw_url = item.get("url")?.as_str()?.trim();
        let url = url::Url::parse(raw_url).ok()?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none()
            || !url.username().is_empty() || url.password().is_some() { return None; }
        let title: String = item.get("title").and_then(Value::as_str).unwrap_or(raw_url)
            .trim().chars().take(300).collect();
        let content: String = item.get("snippet").and_then(Value::as_str).unwrap_or("")
            .trim().chars().take(2000).collect();
        let date = item.get("publishedDate").and_then(|value| {
            if let Some(text) = value.as_str().filter(|text| !text.trim().is_empty()) {
                return Some(text.trim().chars().take(100).collect::<String>());
            }
            let millis = value.as_i64().filter(|value| *value > 0)?;
            chrono::DateTime::from_timestamp_millis(millis).map(|date| date.to_rfc3339())
        });
        Some(json!({ "url": url.as_str(), "title": title, "content": content, "publishedDate": date }))
    }).collect();
    json!({ "sources": rows })
}
