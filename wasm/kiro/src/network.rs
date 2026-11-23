//! HTTP 只走受控 SDK，使用原生端点/认证字段，拒绝跳转或任意外部 IdP 来源。
//! 连接错误不附 URL、代理口令、code 或 token；调试报文脱敏由主程序负责。

use crate::{credentials::Credentials, errors};
use agent2api_wasm_sdk::{http, json, Error, Result, Value};

pub fn header<'a>(headers: &'a Value, name: &str) -> &'a str {
    headers
        .as_object()
        .into_iter()
        .flatten()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .and_then(|(_, value)| value.as_str())
        .unwrap_or("")
}
pub fn auth(c: &Credentials) -> Value {
    let mut headers = json!({ "Authorization": format!("Bearer {}", c.access),
        "Accept": "application/json", "Content-Type": "application/json" });
    if let Some(kind) = c.token_type() {
        headers["tokentype"] = json!(kind);
    }
    headers
}
pub fn read(response: http::Response, secrets: &[&str], limit: usize) -> Result<Value> {
    let status = response.status;
    let mime = header(&response.headers, "content-type").to_ascii_lowercase();
    if mime.contains("text/html") || mime.contains("application/xhtml+xml") {
        return Err(
            Error::http(502, "Kiro 返回了网页而非接口数据，请检查账号和代理")
                .with_code("kiro_upstream"),
        );
    }
    let bytes = response.bytes(limit).map_err(errors::transport)?;
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) if !(200..300).contains(&status) => {
            let text = std::str::from_utf8(&bytes).unwrap_or("");
            return Err(errors::upstream(status, &json!(text), secrets));
        }
        Err(_) => return Err(Error::http(502, "Kiro 没有返回有效 JSON")),
    };
    if !(200..300).contains(&status) || value.get("error").is_some_and(|v| !v.is_null()) {
        return Err(errors::upstream(status, &value, secrets));
    }
    if !value.is_object() {
        return Err(Error::http(502, "Kiro 没有返回有效对象"));
    }
    Ok(value)
}
pub fn get(c: &Credentials, url: &str) -> Result<Value> {
    read(
        http::open_timeout("GET", url, auth(c), None, 20_000).map_err(errors::transport)?,
        &c.secrets(),
        2 * 1024 * 1024,
    )
}
pub fn post(url: &str, headers: Value, body: &Value, secrets: &[&str]) -> Result<Value> {
    if http::json_exceeds(body, 14 * 1024 * 1024)? {
        return Err(
            Error::http(413, "Kiro 请求超过受控传输上限").with_code("plugin_request_too_large")
        );
    }
    read(
        http::open_timeout("POST", url, headers, Some(body), 20_000).map_err(errors::transport)?,
        secrets,
        2 * 1024 * 1024,
    )
}
pub fn aws_json(c: &Credentials, url: &str, target: &str, body: &Value) -> Result<Value> {
    let mut headers = auth(c);
    headers["Content-Type"] = json!("application/x-amz-json-1.0");
    headers["X-Amz-Target"] = json!(target);
    headers["x-amzn-codewhisperer-optout"] = json!("false");
    post(url, headers, body, &c.secrets())
}
