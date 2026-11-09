use base64::Engine;

use crate::{host, json, Error, Result, Value};

pub struct Response {
    pub status: u16,
    pub headers: Value,
    id: String,
    done: bool,
}

pub fn open(method: &str, url: &str, headers: Value, body: Option<&Value>) -> Result<Response> {
    open_raw(method, url, headers, body.map(|body| body.to_string()).as_deref().map(str::as_bytes))
}

pub fn open_raw(method: &str, url: &str, headers: Value, body: Option<&[u8]>) -> Result<Response> {
    open_params(json!({ "method": method, "url": url, "headers": headers,
        "body": body.map(|body| base64::engine::general_purpose::STANDARD.encode(body)) }))
}

pub fn open_resource(method: &str, url: &str, headers: Value, resource: &str) -> Result<Response> {
    open_params(json!({ "method": method, "url": url, "headers": headers, "body_resource": resource }))
}

pub fn multipart(method: &str, url: &str, headers: Value, fields: Value, files: Vec<Value>) -> Result<Response> {
    open_params(json!({ "method": method, "url": url, "headers": headers, "multipart": { "fields": fields, "files": files } }))
}

fn open_params(params: Value) -> Result<Response> {
    let result = host("http.open", params)?;
    Ok(Response {
        status: result["status"].as_u64().and_then(|status| u16::try_from(status).ok())
            .ok_or_else(|| Error::new("网络回应缺少状态码"))?,
        headers: result["headers"].clone(),
        id: result["id"].as_str().ok_or_else(|| Error::new("网络回应缺少连接标识"))?.to_string(),
        done: false,
    })
}

impl Response {
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
