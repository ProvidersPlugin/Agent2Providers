//! 旧凭证续期可返回 IAM；迁移成功后不再保留旧刷新令牌。

use serde_json::{json, Value};

use crate::errors::GatewayError;
use crate::http::send_raw;
use crate::logging;

use super::credentials::{RaccoonCredentials, AUTH_REQUEST_TIMEOUT_MS};
use super::{jwt, upstream_errors};

pub(super) async fn call(
    credentials: &RaccoonCredentials,
) -> Result<RaccoonCredentials, GatewayError> {
    let url = format!("{}/refresh", super::auth_api_base());
    let headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        (
            "Authorization".to_string(),
            format!("Bearer {}", jwt::strip_bearer(&credentials.token)),
        ),
    ];
    let body = json!({ "refresh_token": credentials.refresh_token });
    let response = send_raw(
        "POST",
        &url,
        Some(&body),
        &headers,
        None,
        Some(AUTH_REQUEST_TIMEOUT_MS),
    )
    .await
    .map_err(|error| {
        GatewayError::with_status(502, format!("小浣熊刷新 token 请求失败: {error}"))
    })?;
    let payload = response.payload.unwrap_or(Value::Null);
    let code = upstream_errors::business_code(&payload);
    if !response.ok || code.is_some_and(|code| code != 0) {
        return Err(GatewayError::with_status(
            if response.status == 401 || upstream_errors::token_expired(&payload) {
                401
            } else {
                502
            },
            format!(
                "小浣熊刷新接口失败（HTTP {}）: {}",
                response.status,
                upstream_errors::message(&payload)
            ),
        )
        .with_optional_code(code));
    }
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    let access_token = data
        .get("access_token")
        .and_then(Value::as_str)
        .map(jwt::strip_bearer)
        .unwrap_or_default();
    if access_token.is_empty() {
        return Err(
            GatewayError::with_status(401, "小浣熊刷新接口未返回新的 access_token")
                .with_optional_code(code),
        );
    }
    let profile = if jwt::is_iam_token(&access_token) {
        Some(super::iam::user_info(&access_token).await?)
    } else {
        None
    };
    let next_refresh = if profile.is_some() {
        String::new()
    } else {
        data.get("refresh_token")
            .and_then(Value::as_str)
            .map(jwt::strip_bearer)
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| credentials.refresh_token.clone())
    };
    let mut next = credentials.clone();
    next.expires_at = jwt::jwt_expiry_ms(&access_token).or(credentials.expires_at);
    next.user_id = profile
        .as_ref()
        .and_then(|profile| profile.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            jwt::decode_jwt_claims(&access_token)
                .as_ref()
                .map(jwt::extract_user_id)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| credentials.user_id.clone());
    next.token = access_token;
    next.refresh_token = next_refresh;
    logging::verbose(
        "[Raccoon]",
        &format!("账号 {} 的小浣熊 token 已刷新", credentials.id),
    );
    Ok(next)
}
