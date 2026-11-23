//! 分开读取模型／账号令牌；所有网络经宿主，错误不回显令牌、兑换码或设备标识。

use super::{endpoints, errors};
use agent2api_wasm_sdk::{http, json, Error, Result, Value};

pub struct Reply {
    pub status: u16,
    pub body: Value,
}

pub fn headers(token: Option<&str>, account: bool) -> Value {
    let mut headers = json!({ "accept": "application/json", "cache-control": "no-cache" });
    if let Some(token) = token {
        headers["authorization"] = json!(format!("Bearer {token}"));
    }
    if token.is_some() {
        headers["x-qurwork-env"] = json!("prod");
        headers["x-qurwork-version"] = json!(endpoints::VERSION);
    }
    if !account {
        headers["content-type"] = json!("application/json");
    }
    headers
}

pub fn transport(mut error: Error) -> Error {
    let (status, message) = match error.code.as_deref() {
        Some("timeout") => (504, "QuWork 请求超时"),
        Some("aborted") => (499, "QuWork 请求已取消"),
        Some("forbidden") => (403, "QuWork 请求缺少已声明的主程序授权"),
        _ => (502, "无法连接 QuWork，请检查账号代理和网络"),
    };
    error.status = Some(status);
    error.message = message.into();
    error
}

pub fn read(response: http::Response) -> Result<Reply> {
    let status = response.status;
    let bytes = response.bytes(endpoints::MAX_RESPONSE).map_err(transport)?;
    let body = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) if !(200..300).contains(&status) => json!({ "success": false }),
        Err(_) => return Err(Error::http(502, "QuWork 未返回有效的 JSON 数据")),
    };
    Ok(Reply { status, body })
}

pub fn request(method: &str, path: &str, access: &str, body: Option<&Value>) -> Result<Reply> {
    let mut headers = headers(Some(access), true);
    if body.is_some() {
        headers["content-type"] = json!("application/json");
    }
    read(
        http::open(
            method,
            &format!("{}{path}", endpoints::ACCOUNT),
            headers,
            body,
        )
        .map_err(transport)?,
    )
}

pub fn public_config() -> Result<Value> {
    // 此目录是真正公开的；绝不读取账号或将令牌发给图床域名。
    let response = http::open(
        "GET",
        endpoints::CATALOG,
        json!({ "accept": "application/json", "cache-control": "no-cache" }),
        None,
    )
    .map_err(transport)?;
    let reply = read(response)?;
    if !(200..300).contains(&reply.status) {
        return Err(Error::http(502, "QuWork 公开模型目录获取失败"));
    }
    Ok(reply.body)
}

pub fn success(value: &Value) -> bool {
    match value {
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => matches!(value.trim().to_ascii_lowercase().as_str(), "true" | "1"),
        _ => false,
    }
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status) && success(&self.body["success"])
    }
    pub fn unauthorized(&self) -> bool {
        self.status == 401
            || !self.ok() && matches!(message(&self.body).as_str(), "未登录" | "用户未登录")
    }
    pub fn rejected(&self) -> bool {
        // 网关错误、超时和流量限制都可能发生在写请求已送达之后，不能撤销待确认态。
        if matches!(self.status, 408 | 429) || self.status >= 500 {
            return false;
        }
        matches!(self.status, 400 | 401 | 403 | 404 | 405 | 422)
            || (200..300).contains(&self.status)
                && self.body.get("success").is_some()
                && !success(&self.body["success"])
    }
    pub fn data(&self, operation: &str) -> Result<Value> {
        if !self.ok() {
            if self.unauthorized() {
                return Err(errors::account_expired());
            }
            let status = if self.status >= 400 { self.status } else { 502 };
            let error = errors::upstream(status, &self.body, "account");
            return Err(if error.code.as_deref() == Some("quwork_upstream") {
                Error::http(status, format!("QuWork {operation}未成功"))
                    .with_code("quwork_upstream")
            } else {
                error
            });
        }
        Ok(self.body.get("data").cloned().unwrap_or(Value::Null))
    }
}

pub fn message(body: &Value) -> String {
    body.pointer("/error/message")
        .or_else(|| body.get("message"))
        .or_else(|| body.get("msg"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(300)
        .collect()
}

pub fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        .filter(|value: &f64| value.is_finite() && *value >= 0.0)
}

pub fn timestamp(value: &Value) -> Option<i64> {
    if let Some(value) = number(value).filter(|value| *value > 0.0) {
        let value = value as i64;
        return Some(if value < 100_000_000_000 {
            value.saturating_mul(1000)
        } else {
            value
        });
    }
    chrono::DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|date| date.timestamp_millis())
}
