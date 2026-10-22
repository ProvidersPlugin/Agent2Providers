//! WorkBuddy 的**账号身份与凭证**：请求头、临期判定、续期。
//!
//! ── 从网关搬来的三个部分 ────────────────────────────────────
//! ```text
//!   build_auth_headers       ← core::auth::AuthService::build_auth_headers
//!   enterprise_headers       ← core::auth::AuthService::enterprise_headers
//!   is_token_expiring        ← core::auth::is_token_expiring（窗口同一常量）
//!   refresh_session          ← core::auth::AuthService::refresh_session
//! ```
//!
//! ── 为什么不搬整个 `AuthService` ─────────────────────────────
//! 网关那个服务还管「当前账号是谁」「单飞刷新表」「needsRelogin 标记落盘」
//! 这些**库侧状态**。插件这边账号在网关的库里，读一次就是一次 RPC，所以
//! 那部分留在网关；插件只保留「拿一份会话，构造头 / 打一次续期接口」这种
//! 纯函数 + 单次请求。
//!
//! ── 续期的并发语义（与网关那份的差别）──────────────────────
//! 网关用 `refresh_flight` 表保证「同一账号同时只有一次真实刷新」，因为
//! refresh token 是**一次性**的（换发即消费）—— 两个并发刷新带同一个 RT 去换，
//! 第二个必然拿到 `12153: refresh token is invalid`，而一旦把它当失败，整条
//! RT 链就死了。
//!
//! 插件侧同样需要这道闸，用 [`crate::flight::Table`]（与网关同一个原语）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::sync::OnceLock;

use serde_json::{json, Value};

use agent2api_provider_protocol::WireProxy as ResolvedProxy;

use crate::errors::GatewayError;
use crate::flight;
use crate::http::{send_raw, ApiResponse};
use crate::logging;

use crate::workbuddy_shared::endpoints::{Context, RESPONSE_CODE_OK};

/// 临期窗口（与网关 `auth_http::REFRESH_WINDOW_MS` 同值：5 分钟）。
pub const REFRESH_WINDOW_MS: f64 = 5.0 * 60.0 * 1000.0;

/// 上游「refresh token 已失效」的业务码。
///
/// ── 为什么在插件里没有判据（迁移时改的）──────────────────────
/// 网关用它做「连续 N 次才判死」的计数（`needsRelogin`），而插件没有那个
/// 计数位置（见 `mod.rs` 的 `refresh_account`）。这个常量保留给排障对照 ——
/// 插件把 12153 原样带在错误的 `upstream_code` 里，用户在界面上看到的是
/// 网关侧拼的「请重新登录」。
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
    let data = unwrap_response(&response, "auth/token/refresh")?;
    let next = refresh_result_from(&data);
    if next.access_token.is_empty() {
        return Err(GatewayError::with_status(502, "token 刷新响应缺少 accessToken"));
    }
    Ok(next)
}

// ─── 单飞刷新表 ────────────────────────────────────────────────

/// 单飞键：`账号 id + refresh token 指纹`。
///
/// 带上 RT 的指纹（而不是只用账号 id）的理由与网关 `refresh_flight_key` 相同：
/// 用户重新登录换了一条 RT 之后，那次刷新不该被上一轮的键挡住。
fn flight_key(account_id: &str, refresh_token: &str) -> String {
    let tail: String = {
        let chars: Vec<char> = refresh_token.chars().collect();
        let start = chars.len().saturating_sub(12);
        chars[start..].iter().collect()
    };
    format!("{account_id}|{tail}")
}

fn flight_table() -> &'static flight::Table<RefreshedAuth> {
    static TABLE: OnceLock<flight::Table<RefreshedAuth>> = OnceLock::new();
    TABLE.get_or_init(flight::Table::new)
}

/// 同账号同时只允许一次真实续期；后到者等这一轮的结果并复用。
///
/// ── 为什么必须有这道闸（issue #215 的事故链）──────────────────
/// refresh token 是**一次性**的（换发即消费）。两个并发刷新带同一个 RT 去换，
/// 第二个必然拿到 `12153: refresh token is invalid`，而它一旦把这个失败当成
/// 「这次刷新失败」，整条 RT 链就死了 —— 之后无论怎么重试都救不回来，只能
/// 重新登录。收敛成一次真实请求之后，这个竞态从源头消失。
pub async fn refresh_once(
    account_id: &str,
    refresh_token: &str,
    auth: &Value,
    context: &Context,
    proxy: Option<&ResolvedProxy>,
) -> Result<RefreshedAuth, GatewayError> {
    let key = flight_key(account_id, refresh_token);
    match flight_table().join(&key) {
        flight::Join::Leader(guard) => {
            let result = refresh_session(auth, context, proxy).await;
            guard.finish(result.clone());
            result
        }
        flight::Join::Waiter(waiter) => waiter.wait().await,
    }
}
