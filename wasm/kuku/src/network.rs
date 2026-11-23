//! 受控HTTP，Cookie只发库库主站；passport只发送明确签名参数，禁止带全Cookie。
//! 原生query/body会含会话三件套和jsToken，调试脱敏由宿主完成，错误不回显请求URL。

use crate::{credentials::Credentials, endpoints, errors};
use agent2api_wasm_sdk::{http, json, Error, Result, Value};

pub fn headers(c: &Credentials) -> Value {
    json!({ "Cookie": c.cookie(), "User-Agent": endpoints::USER_AGENT,
        "Referer": format!("{}/genflowpro", endpoints::BASE), "Origin": endpoints::BASE,
        "Accept-Language": "zh-CN,zh;q=0.9", "Accept": "application/json", "Content-Type": "application/json" })
}
pub fn activity_headers(c: &Credentials) -> Value {
    let mut value = headers(c);
    value["Referer"] = json!(endpoints::ACTIVITY_PAGE);
    value
}
pub fn header<'a>(headers: &'a Value, name: &str) -> &'a str {
    headers
        .as_object()
        .into_iter()
        .flatten()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .and_then(|(_, value)| value.as_str())
        .unwrap_or("")
}
pub fn read(response: http::Response, secrets: &[&str], limit: usize) -> Result<Value> {
    let status = response.status;
    let bytes = response.bytes(limit).map_err(errors::transport)?;
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) if !(200..300).contains(&status) => {
            return Err(errors::upstream(status, None, &Value::Null, secrets))
        }
        Err(_) => return Err(Error::http(502, "库库返回非JSON响应").with_code("kuku_protocol")),
    };
    if !(200..300).contains(&status) {
        return Err(errors::upstream(status, None, &value, secrets));
    }
    if !value.is_object() {
        return Err(Error::http(502, "库库响应不是有效对象"));
    }
    Ok(value)
}
pub fn get(url: &str, headers: Value, secrets: &[&str]) -> Result<Value> {
    read(
        http::open("GET", url, headers, None).map_err(errors::transport)?,
        secrets,
        2 * 1024 * 1024,
    )
}
pub fn post(url: &str, headers: Value, body: &Value, secrets: &[&str]) -> Result<Value> {
    if http::json_exceeds(body, 14 * 1024 * 1024)? {
        return Err(
            Error::http(413, "库库请求超过受控传输上限").with_code("plugin_request_too_large")
        );
    }
    read(
        http::open("POST", url, headers, Some(body)).map_err(errors::transport)?,
        secrets,
        2 * 1024 * 1024,
    )
}
pub fn form(url: &str, mut headers: Value, body: &str, secrets: &[&str]) -> Result<Value> {
    if body.len() > 64 * 1024 {
        return Err(Error::http(413, "库库表单超过受控大小上限"));
    }
    headers["Content-Type"] = json!("application/x-www-form-urlencoded;charset=UTF-8");
    read(
        http::open_raw("POST", url, headers, Some(body.as_bytes())).map_err(errors::transport)?,
        secrets,
        1024 * 1024,
    )
}
pub fn activity_page(c: &Credentials) -> Result<String> {
    let mut headers = activity_headers(c);
    headers["Accept"] = json!("text/html,application/xhtml+xml");
    let response =
        http::open("GET", endpoints::ACTIVITY_PAGE, headers, None).map_err(errors::transport)?;
    let status = response.status;
    if !(200..300).contains(&status) {
        return Err(errors::upstream(status, None, &Value::Null, &c.secrets()));
    }
    let bytes = response.bytes(1024 * 1024).map_err(errors::transport)?;
    String::from_utf8(bytes).map_err(|_| Error::http(502, "库库活动页不是有效UTF-8文本"))
}
