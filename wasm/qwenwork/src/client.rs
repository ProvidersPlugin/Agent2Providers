//! 千问办公账户请求是普通 Bearer + qoderwork/1.1.0，续期另带 Login-Version 与请求编号。
//! 不发送 Qoder 账号的 Cosy-Version/ClientType，不接受其他来源，网络错误不回显原文。
use super::{endpoints, errors, util};
use agent2api_wasm_sdk::{http, json, Error, Result, Value};

pub struct Reply {
    pub status: u16,
    pub value: Value,
}
pub fn url(path: &str) -> Result<String> {
    if !path.starts_with('/') || path.starts_with("//") || path.contains(['\\', '\r', '\n', '\0', '#']) {
        return Err(Error::coded("千问办公请求超出国内固定来源", "forbidden"));
    }
    Ok(format!("{}{path}", endpoints::ORIGIN))
}
pub fn transport(mut error: Error, operation: &str) -> Error {
    if !util::terminal(&error) {
        error.message = format!("千问办公{operation}连接失败或响应中断");
        error.code = Some("qwenwork_transport".into());
    }
    error
}
pub fn request(method: &str, path: &str, headers: Value, body: Option<&Value>, operation: &str) -> Result<Reply> {
    let url = url(path)?;
    let result = (|| -> Result<Reply> {
        let response = http::open(method, &url, headers, body)?;
        let status = response.status;
        let bytes = response.bytes(4 * 1024 * 1024)?;
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok(Reply { status, value })
    })();
    result.map_err(|error| transport(error, operation))
}
pub fn accepted(reply: Reply, action: &str) -> Result<Value> {
    if !(200..300).contains(&reply.status) {
        return Err(errors::failure(reply.status, &reply.value));
    }
    if !reply.value.is_object() {
        return Err(Error::http(502, format!("千问办公{action}未返回有效 JSON 对象")));
    }
    if errors::has_error(&reply.value) {
        return Err(errors::failure(reply.status, &reply.value));
    }
    Ok(reply.value)
}
pub fn unwrap_data(value: &Value) -> &Value { value.get("data").filter(|value| value.is_object()).unwrap_or(value) }
pub fn account_headers(token: Option<&str>) -> Value {
    let mut headers = json!({ "User-Agent": endpoints::USER_AGENT, "Accept": "application/json" });
    if let Some(token) = token {
        headers["Authorization"] = json!(format!("Bearer {token}"));
    }
    headers
}
pub fn refresh_headers() -> Result<Value> {
    Ok(
        json!({ "User-Agent": endpoints::USER_AGENT, "Accept": "application/json", "Content-Type": "application/json",
        "Login-Version": "v2", "X-Request-Id": util::uuid()? }),
    )
}
pub fn get(path: &str, token: &str, action: &str) -> Result<Reply> {
    request("GET", path, account_headers(Some(token)), None, action)
}
