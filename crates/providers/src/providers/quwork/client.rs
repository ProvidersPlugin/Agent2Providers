use agent2api_provider_protocol::WireProxy;
use agent2api_provider_sdk::PluginCtx;
use reqwest::{Client, Method};
use serde_json::{json, Value};

use super::endpoints;
use crate::errors::GatewayError;
use crate::http::AccountRequest;

#[derive(Clone)]
pub struct Reply {
    pub status: u16,
    pub body: Value,
}

pub fn client(ctx: &PluginCtx, proxy: Option<&WireProxy>) -> Client {
    crate::http::control_client(proxy, &ctx.env.proxy)
}

pub async fn request(
    client: &Client,
    method: Method,
    path: &str,
    token: &str,
    body: Option<&Value>,
) -> Result<Reply, GatewayError> {
    let mut request = client
        .request(method, format!("{}{path}", endpoints::ACCOUNT_ORIGIN))
        .header("Accept", "application/json")
        .header("Cache-Control", "no-cache")
        .bearer_auth(token);
    for (key, value) in endpoints::headers() {
        request = request.header(key, value);
    }
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request.with_account_user_agent().send().await.map_err(network_error)?;
    read_reply(response, &[token]).await
}

pub async fn public_config(client: &Client) -> Result<Value, GatewayError> {
    let response = client
        .get(endpoints::CATALOG_URL)
        .header("Accept", "application/json")
        .header("Cache-Control", "no-cache")
        .with_account_user_agent().send()
        .await
        .map_err(network_error)?;
    let reply = read_reply(response, &[]).await?;
    if !(200..300).contains(&reply.status) {
        return Err(GatewayError::with_status(502, "QuWork 模型目录获取失败"));
    }
    Ok(reply.body)
}

async fn read_reply(
    mut response: reqwest::Response,
    secrets: &[&str],
) -> Result<Reply, GatewayError> {
    let status = response.status().as_u16();
    if response
        .content_length()
        .is_some_and(|size| size > endpoints::MAX_RESPONSE_BYTES as u64)
    {
        return Err(GatewayError::with_status(502, "QuWork 返回的数据过大"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network_error)? {
        if bytes.len().saturating_add(chunk.len()) > endpoints::MAX_RESPONSE_BYTES {
            return Err(GatewayError::with_status(502, "QuWork 返回的数据过大"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let mut body: Value = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(_) if !(200..300).contains(&status) => {
            json!({ "success": false, "message": format!("上游返回 HTTP {status}") })
        }
        Err(_) => {
            return Err(GatewayError::with_status(
                502,
                "QuWork 未返回有效的 JSON 数据",
            ))
        }
    };
    for path in ["/message", "/msg", "/error/message"] {
        if let Some(value) = body.pointer_mut(path) {
            if let Some(message) = value.as_str() {
                let mut message = message.to_string();
                for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
                    message = message.replace(secret, "[已隐藏]");
                }
                *value = json!(message.chars().take(300).collect::<String>());
            }
        }
    }
    Ok(Reply { status, body })
}

pub fn success(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64().is_some_and(|value| value != 0.0),
        Some(Value::String(value)) => {
            matches!(value.trim().to_ascii_lowercase().as_str(), "true" | "1")
        }
        _ => false,
    }
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status) && success(self.body.get("success"))
    }

    pub fn unauthorized(&self) -> bool {
        self.status == 401
            || (!self.ok() && matches!(message(&self.body).as_str(), "未登录" | "用户未登录"))
    }

    pub fn data(&self, operation: &str) -> Result<Value, GatewayError> {
        if !self.ok() {
            let detail = message(&self.body);
            let status = if self.unauthorized() {
                401
            } else if self.status >= 400 {
                i32::from(self.status)
            } else {
                502
            };
            return Err(GatewayError::with_status(
                status,
                if detail.is_empty() {
                    format!("QuWork {operation}未成功")
                } else {
                    format!("QuWork {operation}未成功：{detail}")
                },
            ));
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

pub fn number(value: Option<&Value>) -> Option<f64> {
    crate::quota::number(value).filter(|value| *value >= 0.0)
}

pub fn timestamp(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(number) = number(Some(value)).filter(|value| *value > 0.0) {
        let number = number as i64;
        return Some(if number < 100_000_000_000 {
            number.saturating_mul(1000)
        } else {
            number
        });
    }
    chrono::DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|date| date.timestamp_millis())
}

fn network_error(error: reqwest::Error) -> GatewayError {
    GatewayError::with_status(
        if error.is_timeout() { 504 } else { 502 },
        if error.is_timeout() {
            "QuWork 请求超时"
        } else {
            "无法连接 QuWork，请检查账号代理和网络"
        },
    )
}
