//! WorkBuddy 原生搜索：官方客户端的 `/agenttool/v1/search`，不经过聊天模型。
//! 请求与结果字段见 magpie `internal/gateway/search_workbuddy.go`；两地区分别认证。

use agent2api_provider_sdk::{PluginCtx, Provider};
use serde_json::{json, Value};
use std::time::Duration;

use crate::{errors::GatewayError, native_search};
use super::{chat_headers, errors, session_endpoint_of, session_of_record, WorkBuddyProvider};

const MAX_QUERY_CHARS: usize = 4000;

pub(super) async fn dispatch(
    provider: &WorkBuddyProvider,
    ctx: &PluginCtx,
    account: &str,
    action: &str,
    payload: &Value,
) -> Result<Value, String> {
    if action == native_search::CAPABILITIES {
        return Ok(native_search::capabilities(MAX_QUERY_CHARS));
    }
    let (query, timeout) = native_search::input(payload, MAX_QUERY_CHARS)
        .map_err(GatewayError::into_control_message)?;
    let key = format!("search:{}:{account}", provider.region.provider_id());
    // 超时包含排队与续期；排到时已超时的查询不会继续向上游发送。
    tokio::time::timeout(timeout, crate::flight::run(&key, || async {
        let ctx = native_search::account_context(ctx, provider.region.provider_id(), account).await?;
        provider.ensure_token(&ctx, account).await.map_err(|_| {
            native_search::rejected(401).into_control_message()
        })?;
        for attempt in 0..2 {
            match search_once(provider, &ctx, account, &query, timeout).await {
                Err(error) if error.status_code == 401 && attempt == 0 => {
                    provider.refresh_token(&ctx, account).await.map_err(|_| {
                        native_search::rejected(401).into_control_message()
                    })?;
                }
                outcome => return outcome.map_err(GatewayError::into_control_message),
            }
        }
        Err(native_search::rejected(401).into_control_message())
    })).await.unwrap_or_else(|_| Err(native_search::timed_out().into_control_message()))
}

async fn search_once(
    provider: &WorkBuddyProvider,
    ctx: &PluginCtx,
    account: &str,
    query: &str,
    timeout: Duration,
) -> Result<Value, GatewayError> {
    let record = crate::store::record(ctx, provider.region.provider_id(), account).await?;
    let session = session_of_record(&record, provider.region);
    let base = session_endpoint_of(&session, provider.region);
    let url = format!("{}/agenttool/v1/search", base.trim_end_matches('/'));
    let client = crate::http::control_client(None, &ctx.env.proxy);
    let mut request = client.post(&url).timeout(timeout).json(&json!({
        "query": query, "type": "text2text", "max_results": 5,
    }));
    for (name, value) in chat_headers(&session, provider.region, Some("application/json")) {
        request = request.header(name, value);
    }
    let response = request.send().await.map_err(native_search::transport)?;
    let (status, value) = native_search::read_response(response).await?;
    let code = value.get("code").and_then(|value| value.as_i64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok())));
    if code == Some(15001) {
        return Err(native_search::rejected(429).upstream_code(15001));
    }
    if !(200..300).contains(&status) || code.is_some_and(|code| code != 0) {
        let status = match errors::classify(status, &value) {
            agent2api_provider_protocol::ErrorClass::TokenExpired { .. } => 401,
            agent2api_provider_protocol::ErrorClass::QuotaLimited { .. } => 429,
            _ if (400..600).contains(&status) => status,
            _ => 502,
        };
        return Err(native_search::rejected(status).with_optional_code(code));
    }
    let items = value.get("results").and_then(Value::as_array)
        .ok_or_else(native_search::invalid_response)?;
    Ok(native_search::sources(items))
}
