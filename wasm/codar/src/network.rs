use agent2api_wasm_sdk::{http, json, Error, Result, Value};

use crate::{endpoints, errors, util};

pub struct Reply {
    pub status: u16,
    pub body: Value,
}

impl Reply {
    pub fn checked(self, protected: &[String]) -> Result<Value> {
        if !(200..300).contains(&self.status) || errors::failed(&self.body) {
            return Err(errors::upstream(self.status, &self.body, protected));
        }
        Ok(self.body)
    }
}

pub fn headers(token: Option<&str>) -> Value {
    let mut headers = json!({ "content-type": "application/json", "accept": "application/json",
        "user-agent": endpoints::USER_AGENT });
    if let Some(token) = token {
        headers["authorization"] = json!(format!("Bearer {token}"));
    }
    headers
}

pub fn open(
    method: &str,
    base: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
    timeout: u64,
) -> Result<http::Response> {
    if !matches!(base, endpoints::API | endpoints::ACCOUNT)
        || !path.starts_with('/')
        || path.starts_with("//")
    {
        return Err(Error::http(400, "科搭网络请求地址未获授权"));
    }
    let protected = token
        .map(|token| vec![token.to_string()])
        .unwrap_or_default();
    let mut headers = headers(token);
    if path == endpoints::RESEARCH {
        headers["user-agent"] = json!(endpoints::RESEARCH_USER_AGENT);
        headers["x-request-id"] = json!(util::nonce()?);
    }
    http::open_timeout(method, &format!("{base}{path}"), headers, body, timeout)
        .map_err(|error| util::redact(error, &protected))
}

pub fn read(response: http::Response, protected: &[String]) -> Result<Reply> {
    let status = response.status;
    let bytes = response
        .bytes(4 * 1024 * 1024)
        .map_err(|error| util::redact(error, protected))?;
    let body = serde_json::from_slice(&bytes).map_err(|_| {
        Error::http(
            if status >= 400 { status } else { 502 },
            format!("科搭接口未返回有效 JSON（HTTP {status}）"),
        )
    })?;
    Ok(Reply { status, body })
}

pub fn request(
    method: &str,
    base: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
    timeout: u64,
) -> Result<Reply> {
    let protected = token
        .map(|token| vec![token.to_string()])
        .unwrap_or_default();
    read(open(method, base, path, token, body, timeout)?, &protected)
}

pub fn checked(
    method: &str,
    base: &str,
    path: &str,
    token: &str,
    body: Option<&Value>,
    timeout: u64,
) -> Result<Value> {
    request(method, base, path, Some(token), body, timeout)?.checked(&[token.to_string()])
}

pub fn data(body: &Value) -> &Value {
    if body["data"].is_object() || body["data"].is_array() || body["data"].is_boolean() {
        &body["data"]
    } else {
        body
    }
}
