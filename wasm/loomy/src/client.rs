//! 网络全部经宿主、沿调用账号/登录 flow 代理；不接收插件自由代理或主机覆盖。

use agent2api_wasm_sdk::{http, json, Error, Result, Value};

use super::{errors, sign, ACCOUNT_BASE, INTEGRATION_BASE};

pub fn read_json(response: http::Response, operation: &str, secrets: &[&str]) -> Result<Value> {
    let status = response.status;
    let bytes = response.bytes(4 * 1024 * 1024).map_err(|mut error| {
        error.message = format!("{operation}响应读取失败");
        error
    })?;
    let payload: Value = serde_json::from_slice(&bytes).map_err(|_| {
        if status == 401 {
            Error::http(401, "Loomy 登录态已失效，请重新登录")
        } else {
            Error::http(
                if (400..600).contains(&status) {
                    status
                } else {
                    502
                },
                format!("{operation}未返回有效 JSON"),
            )
        }
    })?;
    if !(200..300).contains(&status) {
        return Err(errors::http_error(status, &payload, secrets));
    }
    Ok(payload)
}

pub fn signed_post(path: &str, body: &Value, operation: &str, secrets: &[&str]) -> Result<Value> {
    let bytes = body.to_string().into_bytes();
    let headers = sign::headers(path, &bytes)?;
    let response = http::open_raw(
        "POST",
        &format!("{ACCOUNT_BASE}{path}"),
        headers,
        Some(&bytes),
    )
    .map_err(|mut error| {
        error.message = format!("{operation}网络请求失败");
        error
    })?;
    read_json(response, operation, secrets)
}

pub fn token_request(
    method: &str,
    path: &str,
    session: &str,
    body: Option<&Value>,
    operation: &str,
) -> Result<Value> {
    if session.is_empty() {
        return Err(Error::http(401, "Loomy 账号缺少 session"));
    }
    let headers = json!({ "token": session, "Authorization": format!("Bearer {session}"),
        "Content-Type": "application/json", "Accept": "application/json" });
    let response = http::open(method, &format!("{INTEGRATION_BASE}{path}"), headers, body)
        .map_err(|mut error| {
            error.message = format!("{operation}网络请求失败");
            error
        })?;
    let payload = read_json(response, operation, &[session])?;
    if payload.get("error").is_some_and(|error| !error.is_null()) {
        return Err(errors::http_error(502, &payload, &[session]));
    }
    Ok(payload)
}
