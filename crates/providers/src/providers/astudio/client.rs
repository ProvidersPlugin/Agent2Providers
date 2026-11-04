//! AStudio 的两套出网：模型目录（Bearer）与账号面（Cookie）。
//!
//! ── 账号面的信封 ────────────────────────────────────────────
//! `{flag: true, code: 0, data: …}` 才算成功。`flag: false`、非 0 的 `code`、
//! HTTP 401/403、`code: 80000`、`desc` 里含「重新登录」都表示网页会话失效 ——
//! 这类错误**必须**透出去（账号行要显示「需要重新登录」），不能被折叠成
//! 一句「读取失败」。

use serde_json::{json, Value};

use super::accounts::Credentials;
use super::endpoints;
use crate::http;

/// 账号面失败的两种形态。
pub enum StudioError {
    /// 网页会话失效（让用户重新登录）。
    Expired(String),
    /// 其他失败（网络 / 5xx / 业务拒绝）。
    Failed(String),
}

impl StudioError {
    pub fn message(&self) -> &str {
        match self {
            Self::Expired(message) | Self::Failed(message) => message,
        }
    }
}

/// 调一次账号面接口。
pub async fn studio_json(
    credentials: &Credentials,
    path: &str,
    operation: &str,
) -> Result<Value, StudioError> {
    let url = format!(
        "{}{}",
        endpoints::STUDIO_API_BASE,
        path.trim_start_matches('/')
    );
    let client = http::control_client(None, "");
    let response = client
        .get(&url)
        .header("Accept", "application/json")
        .header("Cookie", credentials.cookie_header())
        .send()
        .await
        .map_err(|error| {
            StudioError::Failed(format!("{operation}网络失败：{}", http::describe(&error)))
        })?;
    let status = response.status().as_u16();
    if status == 401 || status == 403 {
        return Err(StudioError::Expired(format!(
            "AStudio 会话已失效（HTTP {status}），请重新登录 AStudio"
        )));
    }
    let text = response.text().await.unwrap_or_default();
    let payload: Value = serde_json::from_str(&text)
        .map_err(|_| StudioError::Failed(format!("{operation}返回的不是 JSON（HTTP {status}）")))?;
    let envelope = payload.as_object().cloned().unwrap_or_default();
    let code = envelope.get("code").and_then(Value::as_i64);
    let failed = !(200..300).contains(&status)
        || envelope.get("flag").and_then(Value::as_bool) == Some(false)
        || code.is_some_and(|code| code != 0);
    if failed {
        let desc = envelope
            .get("desc")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if code == Some(80000) || desc.contains("重新登录") {
            return Err(StudioError::Expired(
                "AStudio 会话已失效，请重新登录 AStudio".into(),
            ));
        }
        return Err(StudioError::Failed(if desc.is_empty() {
            format!("{operation}失败（HTTP {status}）")
        } else {
            format!("{operation}失败（HTTP {status}：{desc}）")
        }));
    }
    Ok(envelope.get("data").cloned().unwrap_or(Value::Null))
}

/// 拉模型目录（`GET /api/v1/model-manager/models`）。
pub async fn fetch_catalog(key: &str) -> Result<Value, CatalogError> {
    let client = http::control_client(None, "");
    let response = client
        .get(endpoints::MODEL_MANAGER_URL)
        .header("Authorization", format!("Bearer {key}"))
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|error| CatalogError::Failed(http::describe(&error)))?;
    let status = response.status().as_u16();
    if status == 401 || status == 403 {
        return Err(CatalogError::Expired);
    }
    if !(200..300).contains(&status) {
        return Err(CatalogError::Failed(format!("HTTP {status}")));
    }
    response
        .json::<Value>()
        .await
        .map_err(|_| CatalogError::Failed("目录不是有效 JSON".into()))
}

/// 目录拉取的失败（与余额查询共用「会话失效」判据）。
pub enum CatalogError {
    Expired,
    Failed(String),
}

/// Responses 请求（流式）。
pub async fn responses(
    credentials: &Credentials,
    body: &Value,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
) -> Result<reqwest::Response, (u16, String)> {
    let client = http::stream_client(proxy, gateway_proxy);
    let response = client
        .post(endpoints::RESPONSES_URL)
        .header("Authorization", format!("Bearer {}", credentials.key))
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .json(body)
        .send()
        .await
        .map_err(|error| {
            (
                502,
                format!("请求 AStudio 失败：{}", http::describe(&error)),
            )
        })?;
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(response);
    }
    let text = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            if text.trim().is_empty() {
                format!("AStudio 返回 HTTP {status}")
            } else {
                text.chars().take(500).collect()
            }
        });
    Err((
        if status == 401 || status == 403 {
            401
        } else if status == 429 {
            429
        } else {
            status
        },
        message,
    ))
}

/// 错误体（网关按 OpenAI chat 错误解析）。
pub fn error_body(status: u16, message: &str) -> Value {
    json!({
        "error": {
            "type": if status == 429 { "rate_limit_error" } else { "api_error" },
            "message": message,
        }
    })
}
