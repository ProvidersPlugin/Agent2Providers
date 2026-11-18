use base64::Engine;

use crate::{host, json, Error, Result, Value};

pub struct Response {
    pub status: u16,
    pub headers: Value,
    pub headers_raw: Vec<(String, String)>,
    id: String,
    done: bool,
}

pub fn open(method: &str, url: &str, headers: Value, body: Option<&Value>) -> Result<Response> {
    open_raw(method, url, headers, body.map(|body| body.to_string()).as_deref().map(str::as_bytes))
}

/// 需要在 manifest 中声明 http.timeout.v1（依赖 http.v1）。
pub fn open_timeout(method: &str, url: &str, headers: Value, body: Option<&Value>, timeout_ms: u64) -> Result<Response> {
    open_raw_timeout(method, url, headers, body.map(|body| body.to_string()).as_deref().map(str::as_bytes), timeout_ms)
}

pub fn open_account(method: &str, path: &str, headers: Value, body: Option<&Value>) -> Result<Response> {
    account_params(method, path, headers, body, None)
}

/// 需要在 manifest 中声明 http.timeout.v1（依赖 http.v1）。
pub fn open_account_timeout(method: &str, path: &str, headers: Value, body: Option<&Value>, timeout_ms: u64) -> Result<Response> {
    account_params(method, path, headers, body, Some(timeout_ms))
}

fn account_params(method: &str, path: &str, headers: Value, body: Option<&Value>, timeout_ms: Option<u64>) -> Result<Response> {
    let body = body.map(|value| base64::engine::general_purpose::STANDARD.encode(value.to_string().as_bytes()));
    open_params("http.open_account", json!({ "method": method, "path": path, "headers": headers, "body": body }), timeout_ms)
}

pub fn open_raw(method: &str, url: &str, headers: Value, body: Option<&[u8]>) -> Result<Response> {
    raw_params(method, url, headers, body, None)
}

/// 需要在 manifest 中声明 http.timeout.v1（依赖 http.v1）。
pub fn open_raw_timeout(method: &str, url: &str, headers: Value, body: Option<&[u8]>, timeout_ms: u64) -> Result<Response> {
    raw_params(method, url, headers, body, Some(timeout_ms))
}

fn raw_params(method: &str, url: &str, headers: Value, body: Option<&[u8]>, timeout_ms: Option<u64>) -> Result<Response> {
    open_params("http.open", json!({ "method": method, "url": url, "headers": headers,
        "body": body.map(|body| base64::engine::general_purpose::STANDARD.encode(body)) }), timeout_ms)
}

pub fn open_resource(method: &str, url: &str, headers: Value, resource: &str) -> Result<Response> {
    resource_params(method, url, headers, resource, None)
}

/// 需要在 manifest 中声明 http.timeout.v1（依赖 http.v1）。
pub fn open_resource_timeout(method: &str, url: &str, headers: Value, resource: &str, timeout_ms: u64) -> Result<Response> {
    resource_params(method, url, headers, resource, Some(timeout_ms))
}

fn resource_params(method: &str, url: &str, headers: Value, resource: &str, timeout_ms: Option<u64>) -> Result<Response> {
    open_params("http.open", json!({ "method": method, "url": url, "headers": headers, "body_resource": resource }), timeout_ms)
}

pub fn multipart(method: &str, url: &str, headers: Value, fields: Value, files: Vec<Value>) -> Result<Response> {
    multipart_params(method, url, headers, fields, files, None)
}

/// 需要在 manifest 中声明 http.timeout.v1（依赖 http.v1）。
pub fn multipart_timeout(method: &str, url: &str, headers: Value, fields: Value, files: Vec<Value>, timeout_ms: u64) -> Result<Response> {
    multipart_params(method, url, headers, fields, files, Some(timeout_ms))
}

fn multipart_params(method: &str, url: &str, headers: Value, fields: Value, files: Vec<Value>, timeout_ms: Option<u64>) -> Result<Response> {
    open_params("http.open", json!({ "method": method, "url": url, "headers": headers,
        "multipart": { "fields": fields, "files": files } }), timeout_ms)
}

fn open_params(method: &str, mut params: Value, timeout_ms: Option<u64>) -> Result<Response> {
    if let Some(timeout_ms) = timeout_ms { params["timeout_ms"] = json!(timeout_ms); }
    response(host(method, params)?)
}

fn response(result: Value) -> Result<Response> {
    // 旧宿主没有 headers_raw 时按单值头兼容；已有多值字段必须形状正确，不静默丢 Cookie。
    let headers_raw = match result.get("headers_raw") {
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|_| Error::new("网络回应的多值响应头格式无效"))?,
        None => result["headers"].as_object().into_iter().flatten().filter_map(|(name, value)| {
            value.as_str().map(|value| (name.clone(), value.to_string()))
        }).collect(),
    };
    Ok(Response {
        status: result["status"].as_u64().and_then(|status| u16::try_from(status).ok())
            .ok_or_else(|| Error::new("网络回应缺少状态码"))?,
        headers: result["headers"].clone(),
        headers_raw,
        id: result["id"].as_str().ok_or_else(|| Error::new("网络回应缺少连接标识"))?.to_string(),
        done: false,
    })
}

impl Response {
    pub fn header_values(&self, name: &str) -> Vec<&str> {
        self.headers_raw.iter().filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str()).collect()
    }

    pub fn read(&mut self) -> Result<Option<Vec<u8>>> {
        if self.done { return Ok(None); }
        let result = host("http.read", json!({ "id": self.id }))?;
        if result["done"].as_bool() == Some(true) { self.done = true; return Ok(None); }
        let bytes = base64::engine::general_purpose::STANDARD.decode(result["data"].as_str().unwrap_or(""))
            .map_err(|_| Error::new("网络回应编码无效"))?;
        Ok(Some(bytes))
    }

    pub fn bytes(mut self, limit: usize) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        while let Some(chunk) = self.read()? {
            if output.len().saturating_add(chunk.len()) > limit { return Err(Error::new("上游回应超过大小限制")); }
            output.extend(chunk);
        }
        Ok(output)
    }

    pub fn json(self) -> Result<Value> {
        let status = self.status;
        let bytes = self.bytes(4 * 1024 * 1024)?;
        if !(200..300).contains(&status) { return Err(Error::http(status, format!("上游接口返回 HTTP {status}"))); }
        serde_json::from_slice(&bytes).map_err(|_| Error::new("上游接口未返回有效 JSON"))
    }
}

impl Drop for Response {
    fn drop(&mut self) {
        if !self.done { let _ = host("http.close", json!({ "id": self.id })); }
    }
}
