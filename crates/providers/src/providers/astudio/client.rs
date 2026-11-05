//! AStudio 的两套出网：模型目录（Bearer）与账号面（Cookie）。
//!
//! ── 账号面的信封 ────────────────────────────────────────────
//! `{flag: true, code: 0, data: …}` 才算成功。HTTP 401/403、`code: 80000`、
//! `desc` 里含「重新登录」表示网页会话失效；其余 `flag: false` 或非 0 的
//! `code` 是业务失败，都必须透出去，不能被折叠成「余额无变化」。

use reqwest::Method;
use serde_json::{json, Value};

use super::accounts::Credentials;
use super::endpoints;
use crate::http;

/// 桌面端版本（安装包 `package.json` 的 version，用于 `studioVersion` 头）。
const STUDIO_VERSION: &str = "3.5.1";

/// 账号面请求共用桌面端身份头，登录、余额与签到不能各自漏带。
pub(super) fn decorate(mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let client_type = if cfg!(target_os = "windows") {
        Some("21")
    } else if cfg!(target_os = "macos") {
        Some("22")
    } else {
        None
    };
    if let Some(kind) = client_type {
        request = request.header("clientType", kind);
    }
    request.header("studioVersion", STUDIO_VERSION)
}

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

/// 读取账号面信息。
pub async fn studio_json(
    credentials: &Credentials,
    path: &str,
    operation: &str,
) -> Result<Value, StudioError> {
    studio_request(credentials, Method::GET, path, operation).await
}

/// 复用桌面端启动时的每日登录动作：带 Cookie 的 POST，不附带请求体。
pub async fn init_app(credentials: &Credentials) -> Result<(), StudioError> {
    let value = studio_request(
        credentials,
        Method::POST,
        endpoints::INIT_APP,
        "每日登录",
    )
    .await?;
    match value.get("banned").and_then(Value::as_bool) {
        Some(false) => Ok(()),
        Some(true) => Err(StudioError::Failed("AStudio 账号已被封禁，无法签到".into())),
        None => Err(StudioError::Failed("每日登录返回的账号状态不完整".into())),
    }
}

async fn studio_request(
    credentials: &Credentials,
    method: Method,
    path: &str,
    operation: &str,
) -> Result<Value, StudioError> {
    let url = format!(
        "{}{}",
        endpoints::STUDIO_API_BASE,
        path.trim_start_matches('/')
    );
    let client = http::control_client(None, "");
    let request = client
        .request(method, &url)
        .header("Accept", "application/json")
        .header("Cookie", credentials.cookie_header())
        .timeout(std::time::Duration::from_secs(20));
    let response = decorate(request)
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
    let text = response.text().await.map_err(|error| {
        StudioError::Failed(format!("读取{operation}响应失败：{}", http::describe(&error)))
    })?;
    let payload: Value = serde_json::from_str(&text)
        .map_err(|_| StudioError::Failed(format!("{operation}返回的不是 JSON（HTTP {status}）")))?;
    let envelope = payload
        .as_object()
        .ok_or_else(|| StudioError::Failed(format!("{operation}返回的结果格式异常")))?;
    let code = envelope.get("code").and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_str().and_then(|text| text.trim().parse::<i64>().ok()))
    });
    let flag = envelope.get("flag").and_then(Value::as_bool);
    let failed = !(200..300).contains(&status)
        || flag == Some(false)
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
    if flag != Some(true) || code != Some(0) {
        return Err(StudioError::Failed(format!("{operation}返回的成功状态不完整")));
    }
    envelope
        .get("data")
        .filter(|value| !value.is_null())
        .cloned()
        .ok_or_else(|| StudioError::Failed(format!("{operation}返回的数据不完整")))
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
