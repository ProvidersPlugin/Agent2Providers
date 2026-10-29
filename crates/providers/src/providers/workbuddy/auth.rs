//! WorkBuddy 的账号身份与凭证：请求头、临期判定、单次续期。
//!
//! 本模块不读写账号库。一次性 refresh token 的并发保护与落盘由
//! `super::refresh` 统一负责，避免上游换发后、写回前再次使用旧凭证。

use serde_json::{json, Value};

use agent2api_provider_protocol::WireProxy as ResolvedProxy;

use crate::errors::GatewayError;
use crate::http::{send_raw, ApiResponse};
use crate::logging;

use crate::workbuddy_shared::endpoints::{Context, RESPONSE_CODE_OK};

/// 临期窗口（与网关 `auth_http::REFRESH_WINDOW_MS` 同值：5 分钟）。
pub const REFRESH_WINDOW_MS: f64 = 5.0 * 60.0 * 1000.0;

/// 上游「refresh token 已失效」的业务码；插件如实上报，不在这里永久禁用账号。
#[allow(dead_code)]
pub const CODE_REFRESH_TOKEN_INVALID: i64 = 12153;

/// 一次续期的结果（与网关 `RefreshedAuth` 同形）。
#[derive(Clone, Debug, Default)]
pub struct RefreshedAuth {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<f64>,
    pub refresh_expires_at: Option<f64>,
}

/// 转发请求上的鉴权头（网关 `AuthService::build_auth_headers` 的逐字副本）。
///
/// 与计费版的 [`super::billing::build_headers`] **不能互相替换**：那版固定带
/// `Content-Type` / `Accept`、`X-User-Id` 缺省给空串、无 token 也给 `Bearer `，
/// 且不带 `X-Department-Info`。这些差别是上游两个 buildHeaders 各自的实现。
pub fn build_auth_headers(session: &Value) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = Vec::new();
    let auth = session.get("auth").cloned().unwrap_or(Value::Null);
    let account = session.get("account").cloned().unwrap_or(Value::Null);

    if let Some(uid) = account.get("uid").and_then(Value::as_str) {
        if !uid.is_empty() {
            headers.push(("X-User-Id".to_string(), uid.to_string()));
        }
    }
    if let Some(token) = auth.get("accessToken").and_then(Value::as_str) {
        if !token.is_empty() {
            headers.push(("Authorization".to_string(), format!("Bearer {token}")));
        }
    }
    if let Some(enterprise_id) = account.get("enterpriseId").and_then(Value::as_str) {
        if !enterprise_id.is_empty() {
            headers.push(("X-Enterprise-Id".to_string(), enterprise_id.to_string()));
            headers.push(("X-Tenant-Id".to_string(), enterprise_id.to_string()));
        }
    }
    if let Some(domain) = auth.get("domain").and_then(Value::as_str) {
        if !domain.is_empty() {
            headers.push(("X-Domain".to_string(), domain.to_string()));
        }
    }
    if let Some(department) = account.get("departmentFullName").and_then(Value::as_str) {
        if !department.is_empty() {
            headers.push(("X-Department-Info".to_string(), department.to_string()));
        }
    }
    headers
}

/// 企业扩展头（网关 `enterprise_headers` 的逐字副本）：总是带 `X-Domain`
/// （取 `auth.domain`，缺失时回退端点 authority）。
fn enterprise_headers(auth: &Value, context: &Context) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(enterprise_id) = auth
        .get("account")
        .and_then(|account| account.get("enterpriseId"))
        .and_then(Value::as_str)
    {
        if !enterprise_id.is_empty() {
            headers.push(("X-Enterprise-Id".to_string(), enterprise_id.to_string()));
            headers.push(("X-Tenant-Id".to_string(), enterprise_id.to_string()));
        }
    }
    let domain = auth
        .get("domain")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| context.authority());
    if !domain.is_empty() {
        headers.push(("X-Domain".to_string(), domain));
    }
    headers
}

/// 凭证是否临期（窗口 [`REFRESH_WINDOW_MS`]）。
///
/// 转发链路的懒刷新与后台维护共用这一个判据 —— 两处各写一个窗口，会出现
/// 「维护认为不需要刷、请求链路却认为临期」的分叉，排障时极难发现。
pub fn is_token_expiring(expires_at: f64) -> bool {
    if expires_at <= 0.0 {
        return false;
    }
    expires_at - REFRESH_WINDOW_MS <= logging::now_ms() as f64
}

/// 把刷新响应补上 `expiresAt`（`expiresIn` 秒 → 毫秒时间戳）。
///
/// 与网关 `with_expires_at` 同口径：显式给了 `expiresAt` 就用它，否则按
/// `expiresIn` 现算；两者都没有时留 0（调用方据此保留原值）。
fn with_expires_at(mut auth: Value) -> Value {
    let now = logging::now_ms() as f64;
    let Some(object) = auth.as_object_mut() else {
        return auth;
    };
    let expires_in = object
        .get("expiresIn")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let refresh_expires_in = object
        .get("refreshExpiresIn")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let expires_at = object
        .get("expiresAt")
        .and_then(Value::as_f64)
        .filter(|value| *value != 0.0)
        .unwrap_or_else(|| {
            if expires_in != 0.0 {
                now + expires_in * 1000.0
            } else {
                0.0
            }
        });
    let refresh_expires_at = object
        .get("refreshExpiresAt")
        .and_then(Value::as_f64)
        .filter(|value| *value != 0.0)
        .unwrap_or_else(|| {
            if refresh_expires_in != 0.0 {
                now + refresh_expires_in * 1000.0
            } else {
                0.0
            }
        });
    if let Some(object) = auth.as_object_mut() {
        object.insert("expiresAt".to_string(), json!(expires_at));
        object.insert("refreshExpiresAt".to_string(), json!(refresh_expires_at));
    }
    auth
}

/// 刷新响应 → [`RefreshedAuth`]（字段缺失时给 None，让调用方保留原值）。
fn refresh_result_from(data: &Value) -> RefreshedAuth {
    let enriched = with_expires_at(data.clone());
    RefreshedAuth {
        access_token: enriched
            .get("accessToken")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        refresh_token: enriched
            .get("refreshToken")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        expires_at: enriched
            .get("expiresAt")
            .and_then(Value::as_f64)
            .filter(|value| *value > 0.0),
        refresh_expires_at: enriched
            .get("refreshExpiresAt")
            .and_then(Value::as_f64)
            .filter(|value| *value > 0.0),
    }
}

/// 校验一次上游响应（网关 `auth_http::unwrap_response` 的插件侧等价物）。
///
/// 三档：非 JSON 且非 2xx、非 2xx（取 `error.message` / `message` / `msg`）、
/// 以及 2xx 但业务码非 0。返回 `data` 段。
fn unwrap_response(response: &ApiResponse, api_name: &str) -> Result<Value, GatewayError> {
    let payload = response.payload.as_ref();
    if payload.is_none() && !response.ok {
        return Err(GatewayError::with_status(
            i32::from(response.status),
            format!("{api_name} 返回非 JSON 响应（HTTP {}）", response.status),
        ));
    }
    if !response.ok {
        let message = payload
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .or_else(|| value.get("message").and_then(Value::as_str))
                    .or_else(|| value.get("msg").and_then(Value::as_str))
            })
            .unwrap_or("")
            .to_string();
        let message = if message.is_empty() {
            format!("HTTP {}", response.status)
        } else {
            message
        };
        let code = payload
            .and_then(|value| value.get("code"))
            .and_then(Value::as_i64);
        return Err(GatewayError::with_status(
            i32::from(response.status),
            format!("{api_name} 失败: {message}"),
        )
        .with_optional_code(code));
    }
    if let Some(Value::Number(code)) = payload.and_then(|value| value.get("code")) {
        if let Some(code) = code.as_i64() {
            if code != RESPONSE_CODE_OK {
                let detail = payload
                    .and_then(|value| value.get("message").and_then(Value::as_str))
                    .or_else(|| {
                        payload.and_then(|value| value.get("msg").and_then(Value::as_str))
                    })
                    .unwrap_or("")
                    .to_string();
                return Err(GatewayError::with_status(
                    502,
                    format!("{api_name} 失败（{code}）: {detail}"),
                )
                .upstream_code(code));
            }
        }
    }
    Ok(payload
        .and_then(|value| value.get("data").cloned())
        .unwrap_or(Value::Null))
}

/// 单次续期（网关 `AuthService::refresh_session` 的逐字副本）：不读库、不写回。
///
/// `auth` 是会话里的 `auth` 子对象（`refreshToken` / `domain` / `account`）；
/// `context` 决定打哪个站点（多账号必须用该账号自己的 endpoint）。
pub async fn refresh_session(
    auth: &Value,
    context: &Context,
    proxy: Option<&ResolvedProxy>,
) -> Result<RefreshedAuth, GatewayError> {
    let refresh_token = auth
        .get("refreshToken")
        .and_then(Value::as_str)
        .unwrap_or("");
    if refresh_token.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "缺少 refreshToken，无法续期，请重新登录",
        ));
    }
    let mut headers = enterprise_headers(auth, context);
    headers.push(("X-Refresh-Token".to_string(), refresh_token.to_string()));
    headers.push(("X-Auth-Refresh-Source".to_string(), "plugin".to_string()));
    let url = context.auth_url("/auth/token/refresh");
    let response = send_raw("POST", &url, Some(&json!({})), &headers, proxy, None)
        .await
        .map_err(|error| {
            if error.is_timeout() {
                GatewayError::with_status(504, "token 续期超时")
            } else {
                GatewayError::with_status(502, format!("token 续期失败: {error}"))
            }
        })?;
    // ── 404 / 405 单独说清楚（issue #381）─────────────────────────
    // 续期端点存在（社区实测它回 400「refreshToken is empty」），所以 404
    // 几乎只意味着一件事：**这个账号的 endpoint 指错了站点**（例如国际版
    // 账号的 endpoint 是空串，插件回落到国内版）。这种情况下重新登录、重新
    // 导入凭证都修不好 —— 必须改端点。原来它和「凭证被作废」共用一句话，
    // 把用户往错误的方向带。
    if matches!(response.status, 404 | 405) {
        return Err(GatewayError::with_status(
            i32::from(response.status),
            format!(
                "续期接口不存在（HTTP {}，请求地址 {url}）：通常是账号的端点（endpoint）\
                 与账号所在地区不符，而不是凭证失效。请检查该账号的 endpoint 配置\
                 （国际版应为 https://www.workbuddy.ai）",
                response.status
            ),
        ));
    }
    let data = unwrap_response(&response, "auth/token/refresh")?;
    let next = refresh_result_from(&data);
    if next.access_token.is_empty() {
        return Err(GatewayError::with_status(502, "token 刷新响应缺少 accessToken"));
    }
    Ok(next)
}
