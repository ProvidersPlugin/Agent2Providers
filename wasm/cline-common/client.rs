//! 标准 Cline 请求只走宿主；错误与管理结果不回显授权码、令牌或上游原始诊断。

use super::{endpoints, errors};
use agent2api_wasm_sdk::{http, json, Error, Result, Value};

pub struct Reply {
    pub status: u16,
    pub body: Value,
}

pub fn transport(error: Error) -> Error {
    let (status, code, message) = match error.code.as_deref() {
        Some("timeout") => (504, "timeout", "Cline 请求超时"),
        Some("aborted") => (499, "aborted", "Cline 请求已取消"),
        Some("forbidden") => (403, "forbidden", "Cline 请求缺少主程序授权"),
        _ => (
            502,
            "cline_transport",
            "Cline 网络请求失败，请检查代理或稍后重试",
        ),
    };
    // 宿主/辅助返回的错误码也可能携带诊断 JSON，不连同原错误向外透传。
    Error::http(status, message).with_code(code)
}

pub fn read(response: http::Response) -> Result<Reply> {
    let status = response.status;
    let bytes = response.bytes(endpoints::MAX_RESPONSE).map_err(transport)?;
    let body = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) if !(200..300).contains(&status) => {
            json!({ "message": String::from_utf8_lossy(&bytes) })
        }
        Err(_) => return Err(Error::http(502, "Cline 返回的数据不是有效 JSON")),
    };
    Ok(Reply { status, body })
}

pub fn json_headers(token: Option<&str>, product: bool) -> Value {
    let mut headers = json!({ "accept": "application/json", "content-type": "application/json" });
    if let Some(token) = token {
        headers["authorization"] = json!(format!("Bearer {token}"));
    }
    if product {
        headers["x-client-type"] = json!(endpoints::CLIENT_TYPE);
    }
    headers
}

pub fn request(
    method: &str,
    path: &str,
    token: Option<&str>,
    product: bool,
    body: Option<&Value>,
) -> Result<Reply> {
    read(
        http::open(
            method,
            &format!("{}{path}", endpoints::API),
            json_headers(token, product),
            body,
        )
        .map_err(transport)?,
    )
}

pub fn form(path: &str, fields: &[(&str, &str)]) -> Result<Reply> {
    let body = fields
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    read(http::open_raw("POST", &format!("{}{path}", endpoints::WORKOS),
        json!({ "accept": "application/json", "content-type": "application/x-www-form-urlencoded" }), Some(body.as_bytes())).map_err(transport)?)
}

impl Reply {
    pub fn success(&self) -> bool {
        (200..300).contains(&self.status)
            && self.body.is_object()
            && self.body["type"] != "error"
            && self
                .body
                .get("success")
                .is_none_or(|value| value.as_bool() == Some(true))
            && self
                .body
                .get("error")
                .is_none_or(|value| value.is_null() || value.as_str() == Some(""))
    }
    pub fn data(&self) -> Result<Value> {
        if !self.success() {
            if (200..300).contains(&self.status) {
                if let Some(error) = errors::event(&self.body) {
                    return Err(error);
                }
            }
            return Err(errors::upstream(
                if self.status >= 400 { self.status } else { 502 },
                &self.body,
            ));
        }
        let data = self.body.get("data").unwrap_or(&self.body);
        if !data.is_object() {
            return Err(Error::http(502, "Cline 返回的数据结构无效"));
        }
        if let Some(error) = errors::event(data) {
            return Err(error);
        }
        Ok(data.clone())
    }
}

pub fn encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}
