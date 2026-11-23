//! Kiro 独立网页搜索（InvokeMCP），请求按账号的地区和认证方式构造。
//! 参考 magpie-community/plugins 的 Kiro 适配：OAuth 的 profileArn 同时在体与头；
//! API key 不带 profile。只解析服务返回的来源，不创建对话，也不运行模型。

use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};
use std::time::Duration;

use crate::{errors::GatewayError, native_search};
use crate::http::AccountRequest;
use super::{credentials::Credentials, endpoints, refresh, PROVIDER_ID};

const MAX_QUERY_CHARS: usize = 200;
const MCP_TARGET: &str = "AmazonCodeWhispererStreamingService.InvokeMCP";

pub(super) async fn dispatch(
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
    let key = format!("search:{PROVIDER_ID}:{account}");
    tokio::time::timeout(timeout, crate::flight::run(&key, || async {
        let ctx = native_search::account_context(ctx, PROVIDER_ID, account).await?;
        let (_, mut credentials) = refresh::ensure(&ctx, account).await.map_err(|_| {
            native_search::rejected(401).into_control_message()
        })?;
        for attempt in 0..2 {
            match search_once(&ctx, &credentials, &query, timeout).await {
                Err(error) if error.status_code == 401 && attempt == 0 && !credentials.is_api_key() => {
                    let (_, refreshed) = refresh::force_refresh(&ctx, account).await.map_err(|_| {
                        native_search::rejected(401).into_control_message()
                    })?;
                    credentials = refreshed;
                }
                outcome => return outcome.map_err(GatewayError::into_control_message),
            }
        }
        Err(native_search::rejected(401).into_control_message())
    })).await.unwrap_or_else(|_| Err(native_search::timed_out().into_control_message()))
}

async fn search_once(
    ctx: &PluginCtx,
    credentials: &Credentials,
    query: &str,
    timeout: Duration,
) -> Result<Value, GatewayError> {
    if credentials.access.is_empty() { return Err(native_search::rejected(401)); }
    if !credentials.is_api_key() && credentials.profile.is_empty() {
        return Err(GatewayError::with_status(400, "Kiro 账号缺少搜索所需的 profile，请重新登录")
            .with_code("configuration"));
    }
    let mut body = json!({
        "jsonrpc": "2.0", "id": crate::ids::uuid_v4(), "method": "tools/call",
        "params": { "name": "web_search", "arguments": { "query": query } },
    });
    if !credentials.is_api_key() { body["profileArn"] = json!(credentials.profile); }
    let url = format!("{}/", endpoints::runtime(&credentials.region()));
    let client = crate::http::control_client(None, &ctx.env.proxy);
    let user_agent = "aws-sdk-rust/1.0.0 ua/2.1 os/other lang/rust api/codewhispererstreaming#1.28.3 m/E app/AmazonQ-For-CLI";
    let mut request = client.post(url).timeout(timeout)
        .header("Authorization", format!("Bearer {}", credentials.access))
        .header("Content-Type", "application/x-amz-json-1.0")
        .header("Accept", "application/json")
        .header("x-amz-target", MCP_TARGET)
        .header("x-amzn-codewhisperer-optout", "true")
        .header("amz-sdk-invocation-id", crate::ids::uuid_v4())
        .header("amz-sdk-request", "attempt=1; max=1")
        .header("x-amz-user-agent", user_agent)
        .header("User-Agent", user_agent)
        .json(&body);
    if let Some(kind) = credentials.token_type() { request = request.header("tokentype", kind); }
    if !credentials.is_api_key() {
        request = request.header("x-amzn-kiro-profile-arn", &credentials.profile);
    }
    let response = request.with_account_user_agent().send().await.map_err(native_search::transport)?;
    let (status, value) = native_search::read_response(response).await?;
    if !(200..300).contains(&status) {
        // 只有明确的登录失效才续期；普通 403（无权限）不能当成令牌过期反复打。
        let reason = value.get("reason").and_then(Value::as_str).unwrap_or("");
        let message = value.get("message").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
        let expired = status == 401 || (status == 403
            && (matches!(reason, "INVALID_TOKEN" | "EXPIRED_TOKEN" | "TOKEN_EXPIRED")
                || ["token is expired", "expired token", "invalid token", "invalid bearer token"]
                    .iter().any(|needle| message.contains(needle))));
        return Err(native_search::rejected(if expired { 401 } else { status }));
    }
    parse_result(&value)
}

fn parse_result(value: &Value) -> Result<Value, GatewayError> {
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return Err(native_search::invalid_response());
    }
    let mut result = value.get("result").cloned().ok_or_else(native_search::invalid_response)?;
    if let Some(text) = result.as_str() {
        result = serde_json::from_str(text).map_err(|_| native_search::invalid_response())?;
    }
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(native_search::invalid_response());
    }
    let text = result.get("content").and_then(Value::as_array).into_iter().flatten()
        .find(|item| item.get("type").and_then(Value::as_str) == Some("text"))
        .and_then(|item| item.get("text")).and_then(Value::as_str)
        .ok_or_else(native_search::invalid_response)?;
    let content: Value = serde_json::from_str(text).map_err(|_| native_search::invalid_response())?;
    if content.get("error").is_some_and(|error| !error.is_null()) {
        return Err(native_search::invalid_response());
    }
    let items = content.get("results").and_then(Value::as_array)
        .ok_or_else(native_search::invalid_response)?;
    Ok(native_search::sources(items))
}
