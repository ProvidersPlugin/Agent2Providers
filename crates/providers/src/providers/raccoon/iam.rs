//! IAM 首次迁移通过 user_info 校验；仅业务码 200901 触发一次 iam/confirm。

use serde_json::{json, Value};

use crate::errors::GatewayError;
use crate::http::{send_raw, ApiResponse};

use super::credentials::AUTH_REQUEST_TIMEOUT_MS;

fn result(response: ApiResponse) -> Result<Value, GatewayError> {
    let payload = response.payload.unwrap_or(Value::Null);
    let code = super::upstream_errors::business_code(&payload);
    if response.status == 401 || super::upstream_errors::token_expired(&payload) {
        return Err(
            GatewayError::with_status(401, "小浣熊 IAM 登录态无效，请重新登录")
                .with_optional_code(code),
        );
    }
    if !response.ok || code != Some(0) {
        return Err(
            GatewayError::with_status(502, super::upstream_errors::message(&payload))
                .with_optional_code(code),
        );
    }
    payload
        .get("data")
        .filter(|data| data.is_object())
        .cloned()
        .ok_or_else(|| GatewayError::with_status(502, "小浣熊账号资料响应结构异常"))
}

pub(super) async fn user_info(token: &str) -> Result<Value, GatewayError> {
    super::jwt::validate_iam_token(token)?;
    let mut headers = super::headers::desktop_client_headers();
    headers.push(("Content-Type".to_string(), "application/json".to_string()));
    headers.push(("Authorization".to_string(), format!("Bearer {token}")));
    let base = super::auth_api_base();
    let user_url = format!("{base}/user_info");
    let request = || {
        send_raw(
            "GET",
            &user_url,
            None,
            &headers,
            None,
            Some(AUTH_REQUEST_TIMEOUT_MS),
        )
    };
    let response = request().await.map_err(|error| {
        GatewayError::with_status(502, format!("小浣熊账号资料查询失败: {error}"))
    })?;
    let needs_confirmation = response
        .payload
        .as_ref()
        .and_then(super::upstream_errors::business_code)
        == Some(200_901);
    let data = if needs_confirmation {
        let confirmation = send_raw(
            "POST",
            &format!("{base}/iam/confirm"),
            Some(&json!({})),
            &headers,
            None,
            Some(AUTH_REQUEST_TIMEOUT_MS),
        )
        .await
        .map_err(|error| GatewayError::with_status(502, format!("小浣熊 IAM 确认失败: {error}")))?;
        let payload = confirmation.payload.unwrap_or(Value::Null);
        if !confirmation.ok || super::upstream_errors::business_code(&payload) != Some(0) {
            return Err(GatewayError::with_status(
                if confirmation.status == 401 || super::upstream_errors::token_expired(&payload) {
                    401
                } else {
                    502
                },
                super::upstream_errors::message(&payload),
            )
            .with_optional_code(super::upstream_errors::business_code(&payload)));
        }
        result(request().await.map_err(|error| {
            GatewayError::with_status(502, format!("小浣熊账号资料查询失败: {error}"))
        })?)?
    } else {
        result(response)?
    };
    if !data
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.trim().is_empty() && id.len() <= 256)
    {
        return Err(GatewayError::with_status(
            502,
            "小浣熊账号资料缺少平台用户 ID",
        ));
    }
    Ok(data)
}
