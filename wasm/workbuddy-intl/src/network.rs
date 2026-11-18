//! 仅固定国际主站的受控HTTP，错误不回显登录state、token或完整响应。
use super::{endpoints, errors};
use agent2api_wasm_sdk::{http, Error, Result, Value};

pub struct Reply {
    pub status: u16,
    pub value: Value,
}
impl Reply {
    pub fn ok(&self) -> bool {
        !errors::failed(self.status, &self.value)
    }
    pub fn data(&self) -> &Value {
        self.value.get("data").unwrap_or(&Value::Null)
    }
    pub fn code(&self) -> Option<i64> {
        errors::code(&self.value)
    }
    pub fn message(&self) -> &str {
        errors::message(&self.value)
    }
    pub fn checked(self, secrets: &[&str]) -> Result<Value> {
        if !self.ok() {
            return Err(errors::upstream(self.status, &self.value, secrets));
        }
        Ok(self.value.get("data").cloned().unwrap_or(Value::Null))
    }
}
pub fn open(
    method: &str,
    origin: &str,
    path: &str,
    mut headers: Value,
    body: Option<&Value>,
) -> Result<http::Response> {
    if body.is_some() {
        headers["Content-Type"] = Value::String("application/json".into());
    }
    http::open(method, &endpoints::url(origin, path)?, headers, body).map_err(|mut e| {
        e.message = "WorkBuddy网络请求未完成".into();
        e
    })
}
pub fn open_timeout(
    method: &str,
    origin: &str,
    path: &str,
    mut headers: Value,
    body: Option<&Value>,
    timeout_ms: u64,
) -> Result<http::Response> {
    if body.is_some() {
        headers["Content-Type"] = Value::String("application/json".into());
    }
    http::open_timeout(
        method,
        &endpoints::url(origin, path)?,
        headers,
        body,
        timeout_ms,
    )
    .map_err(|mut error| {
        error.message = "WorkBuddy网络请求未完成".into();
        error
    })
}
pub fn read(response: http::Response, max: usize) -> Result<Reply> {
    let status = response.status;
    let bytes = response.bytes(max).map_err(|mut e| {
        e.message = "WorkBuddy响应读取失败或过大".into();
        e
    })?;
    let value = serde_json::from_slice(&bytes).map_err(|_| {
        Error::http(
            if (400..600).contains(&status) {
                status
            } else {
                502
            },
            "WorkBuddy未返回合法JSON",
        )
    })?;
    Ok(Reply { status, value })
}
pub fn raw(
    method: &str,
    origin: &str,
    path: &str,
    headers: Value,
    body: Option<&Value>,
) -> Result<Reply> {
    read(open(method, origin, path, headers, body)?, 4 * 1024 * 1024)
}
pub fn raw_timeout(
    method: &str,
    origin: &str,
    path: &str,
    headers: Value,
    body: Option<&Value>,
    timeout_ms: u64,
) -> Result<Reply> {
    read(
        open_timeout(method, origin, path, headers, body, timeout_ms)?,
        4 * 1024 * 1024,
    )
}
pub fn request(
    method: &str,
    origin: &str,
    path: &str,
    headers: Value,
    body: Option<&Value>,
    secrets: &[&str],
) -> Result<Value> {
    raw(method, origin, path, headers, body)?.checked(secrets)
}
