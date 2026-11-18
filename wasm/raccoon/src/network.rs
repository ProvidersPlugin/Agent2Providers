//! 所有网络均为固定主站HTTPS；错误不回显令牌、授权码或完整响应体。

use super::{endpoints, errors, jwt};
use agent2api_wasm_sdk::{http, json, Error, Result, Value};

pub struct Reply {
    pub status: u16,
    pub value: Value,
}

pub fn headers(token: &str, org: &str, mut extra: Value) -> Result<Value> {
    if !extra.is_object() {
        return Err(Error::new("请求头必须是JSON对象"));
    }
    if !token.is_empty() {
        extra["Authorization"] = json!(format!("Bearer {}", jwt::strip(token)));
    }
    if !org.is_empty() && org != "personal" {
        if org.len() > 256 || !org.is_ascii() || org.bytes().any(|byte| byte < 32 || byte == 127) {
            return Err(Error::http(400, "小浣熊组织身份无效，请重新导入登录态"));
        }
        extra["X-Org-Code"] = json!(org);
    }
    Ok(extra)
}
pub fn url(path: &str) -> Result<String> {
    if !path.starts_with("/api/web/")
        || path.contains('#')
        || path.contains('\\')
        || path.chars().any(char::is_control)
    {
        return Err(Error::http(400, "小浣熊请求路径无效"));
    }
    Ok(format!("{}{path}", endpoints::ORIGIN))
}
pub fn open(
    method: &str,
    path: &str,
    token: &str,
    org: &str,
    extra: Value,
    body: Option<&Value>,
) -> Result<http::Response> {
    let mut headers = headers(token, org, extra)?;
    if body.is_some() {
        headers["Content-Type"] = json!("application/json");
    }
    http::open(method, &url(path)?, headers, body).map_err(network_error)
}
fn network_error(mut error: Error) -> Error {
    error.message = "小浣熊网络请求未完成".into();
    error
}
pub fn open_timeout(
    method: &str,
    path: &str,
    token: &str,
    org: &str,
    extra: Value,
    body: Option<&Value>,
    timeout_ms: u64,
) -> Result<http::Response> {
    let mut headers = headers(token, org, extra)?;
    if body.is_some() {
        headers["Content-Type"] = json!("application/json");
    }
    http::open_timeout(method, &url(path)?, headers, body, timeout_ms).map_err(network_error)
}
pub fn read(response: http::Response, limit: usize) -> Result<Reply> {
    let status = response.status;
    let bytes = response.bytes(limit).map_err(|mut error| {
        error.message = "小浣熊响应读取失败或超过限制".into();
        error
    })?;
    let value = serde_json::from_slice(&bytes).map_err(|_| {
        Error::http(
            if (400..600).contains(&status) {
                status
            } else {
                502
            },
            "小浣熊未返回有效JSON",
        )
    })?;
    Ok(Reply { status, value })
}
pub fn raw(
    method: &str,
    path: &str,
    token: &str,
    org: &str,
    extra: Value,
    body: Option<&Value>,
) -> Result<Reply> {
    read(
        open_timeout(method, path, token, org, extra, body, 30_000)?,
        4 * 1024 * 1024,
    )
}
pub fn data(reply: Reply, secrets: &[&str], require_code: bool) -> Result<Value> {
    if errors::failed(reply.status, &reply.value)
        || require_code && errors::code(&reply.value) != Some(0)
    {
        return Err(errors::upstream(reply.status, &reply.value, secrets));
    }
    let data = reply
        .value
        .get("data")
        .filter(|value| !value.is_null())
        .cloned();
    Ok(data.unwrap_or(reply.value))
}
pub fn request(
    method: &str,
    path: &str,
    token: &str,
    org: &str,
    extra: Value,
    body: Option<&Value>,
) -> Result<Value> {
    let timeout = if path.starts_with("/api/web/points/v2/bills?") {
        25_000
    } else {
        20_000
    };
    let reply = read(
        open_timeout(method, path, token, org, extra, body, timeout)?,
        4 * 1024 * 1024,
    )?;
    data(reply, &[token], false)
}
