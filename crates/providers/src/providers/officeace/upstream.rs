use std::time::Duration;

use serde_json::Value;

use crate::errors::GatewayError;
use crate::http::{describe_chain, AccountRequest};

use super::credentials::OfficeAceCredential;
use super::signer::{self, Algorithm, SignRequest};

pub const CONTROL_TOKEN_EXPIRED: &str = "officeace_control_token_expired";
pub const RESULT_UNCERTAIN: &str = "officeace_subscription_uncertain";

pub async fn call(
    credential: &OfficeAceCredential,
    method: &str,
    url: &str,
    algorithm: Algorithm,
    body: Option<Value>,
    extra_headers: &[(&str, &str)],
) -> Result<Value, GatewayError> {
    let signing = credential.signing().map_err(GatewayError::from)?;
    if signing.security_token.is_empty() {
        return Err(GatewayError::with_status(400, "OfficeAce 缺少临时安全令牌，请重新登录补齐授权"));
    }
    let payload = body.as_ref().map(Value::to_string).unwrap_or_default();
    let mut headers: Vec<(String, String)> = extra_headers.iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string())).collect();
    if body.is_some() {
        headers.push(("Content-Type".to_string(), "application/json;charset=utf8".to_string()));
    }
    let signed = signer::sign(&SignRequest {
        algorithm, method, url, headers: &headers, body: payload.as_bytes(),
        credential: &signing, region: signer::DEFAULT_REGION, date_override: None,
    }).map_err(GatewayError::from)?;
    let client = crate::http::control_client(None, "direct");
    let mut request = match method {
        "POST" => client.post(url).body(payload),
        _ => client.get(url),
    }.timeout(Duration::from_secs(20));
    for (name, value) in signed {
        request = request.header(name, value);
    }
    let response = request.with_account_user_agent().send().await.map_err(|error| {
        uncertain(method, GatewayError::with_status(502, format!(
            "OfficeAce 请求未能确认结果：{}", clean(&describe_chain(&error), credential),
        )))
    })?;
    let status = response.status().as_u16();
    let text = response.text().await.map_err(|error| {
        uncertain(method, GatewayError::with_status(502, format!(
            "OfficeAce 响应读取失败：{}", clean(&describe_chain(&error), credential),
        )))
    })?;
    let value = serde_json::from_str::<Value>(&text).ok();
    let payload = value.as_ref().unwrap_or(&Value::Null);
    if let Some(error) = response_error(status, payload, credential) {
        return Err(if status >= 500 || status == 408 { uncertain(method, error) } else { error });
    }
    if text.trim().is_empty() && method == "POST" && url.ends_with("/v1/agreement") {
        return Ok(Value::Null);
    }
    value.ok_or_else(|| uncertain(method, GatewayError::with_status(
        502, "OfficeAce 接口响应不是有效 JSON，请查询实际状态后再操作",
    )))
}

pub fn response_error(
    status: u16,
    payload: &Value,
    credential: &OfficeAceCredential,
) -> Option<GatewayError> {
    let code = payload.get("error_code").and_then(Value::as_str).unwrap_or("");
    let detail = ["error_msg", "error_message", "failed_reason", "message"]
        .iter().find_map(|key| payload.get(*key).and_then(Value::as_str)).unwrap_or("");
    let failed = !(200..300).contains(&status)
        || !matches!(code, "" | "0" | "200")
        || payload.get("success").and_then(Value::as_bool) == Some(false)
        || payload.get("status").and_then(Value::as_str) == Some("FAILED");
    if !failed { return None; }
    let lower = detail.to_ascii_lowercase();
    let expired = code == "APIG.0307" || matches!(code, "APIG.0602" | "APIG.1002" | "APIG.0301")
        && ["security token has expired", "access token is expired", "token expires", "token has expired", "token expired"]
            .iter().any(|marker| lower.contains(marker));
    let reason = if expired {
        "OfficeAce 临时凭证已过期，需要刷新登录凭证"
    } else if status == 403 {
        "OfficeAce 拒绝访问，请检查服务开通、账号权限或实名认证；这不代表凭证过期"
    } else if status == 401 {
        "OfficeAce 身份校验失败，请检查登录授权或请求签名；尚不能确认凭证过期"
    } else {
        "OfficeAce 接口拒绝了请求"
    };
    let mut message = format!("{reason}（HTTP {status}）");
    for part in [code, detail].into_iter().filter(|part| !part.is_empty()) {
        message.push_str(&format!("：{}", clean(part, credential)));
    }
    let error_status = if expired { 401 } else if (200..300).contains(&status) { 502 } else { i32::from(status) };
    let error = GatewayError::with_status(error_status, message);
    Some(if expired { error.with_code(CONTROL_TOKEN_EXPIRED) } else { error })
}

fn uncertain(method: &str, error: GatewayError) -> GatewayError {
    if method == "POST" { error.with_code(RESULT_UNCERTAIN) } else { error }
}

fn clean(text: &str, credential: &OfficeAceCredential) -> String {
    let mut value = text.to_string();
    for secret in [
        &credential.access_key_id, &credential.secret_access_key, &credential.security_token,
        &credential.model_app_key, &credential.model_app_secret,
    ].into_iter().filter(|secret| !secret.is_empty()) {
        value = value.replace(secret, "[已隐藏]");
    }
    value.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(240).collect()
}
