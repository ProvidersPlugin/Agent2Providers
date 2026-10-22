//! Cline 的设备授权登录（WorkOS RFC 8628），改造成插件协议的「轮询型」。
//!
//! ── 链路（四步，全部实测核对）───────────────────────────────
//! ```text
//!   1. POST {workos}/user_management/authorize/device    → device_code + user_code
//!   2. 用户在浏览器里输 user_code 并确认                  （网关打开授权页）
//!   3. POST {workos}/user_management/authenticate        → WorkOS 令牌对（轮询）
//!   4. POST {api}/auth/register                          → Cline 会话令牌
//! ```
//!
//! 第 4 步**不能省**：WorkOS 的令牌不能直接打 LLM 接口，必须经 `/auth/register`
//! 换成本家会话令牌（`workos:` 前缀 + 一套自己的 refresh token）。
//!
//! ── 与网关那份的差别（迁移时改的）───────────────────────────
//! 网关的 `poll_and_register` 是**一个阻塞到结束的循环**（`api::session` 在
//! 那个 async 里等）。插件协议里登录是「开始 → 反复轮询」两段，所以拆成：
//!
//! | 网关 | 插件 |
//! |---|---|
//! | `start()` 拿 device_code（只在内部用） | [`login_start`]：把 `user_code` 拼进说明，`flow` = device_code |
//! | `poll_and_register` 内部 while 循环 | [`login_poll`]：每次只轮询**一次**，由网关按间隔调 |
//! | 成功后 `store.add_cline_account` | 返回 `NormalizedAccount`，由网关落库 |
//!
//! 轮询间隔跟着上游的 `interval`（默认 5 秒）。`slow_down` 时上游只是继续回
//! `authorization_pending`，不会失败 —— 插件侧没有跨调用的可变状态，
//! 「间隔变长」这件事无法持久化，这是有意接受的降级。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::time::Duration;

use serde_json::{json, Value};

use agent2api_provider_protocol as proto;

use crate::errors::GatewayError;
use crate::http::{describe_chain, send_raw};
use crate::json::truncate_text;

use super::credentials::{self, WORKOS_BASE_URL, WORKOS_CLIENT_ID};

/// 单次 HTTP 超时（授权接口是交互式的，给短一些）
const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 轮询上限：`expires_in` 是 300 秒，这里给同等上限
const MAX_POLL_MS: u64 = 300_000;

/// 默认轮询间隔（上游返回 `interval`，缺失时用它）
const DEFAULT_INTERVAL_SECONDS: u64 = 5;

/// 设备授权的第一步产物（`login_start` 用它拼说明）。
struct DeviceAuthStart {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in_seconds: u64,
    interval_seconds: u64,
}

/// 发起设备授权（第一步）。
async fn start() -> Result<DeviceAuthStart, GatewayError> {
    let headers = vec![("Accept".to_string(), "application/json".to_string())];
    // 表单体：`client_id=...`。`send_raw` 的 body 参数是 JSON，这里用
    // `form_post` 直发（WorkOS 的 device 端点同样只认 form 编码）。
    let response = form_post(
        &format!("{WORKOS_BASE_URL}/user_management/authorize/device"),
        &format!("client_id={WORKOS_CLIENT_ID}"),
        &headers,
    )
    .await?;
    let status = response.0;
    let payload = response.1;
    if !(200..300).contains(&status) {
        let detail = payload
            .get("error_description")
            .or_else(|| payload.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let detail = if detail.is_empty() {
            truncate_text(&payload.to_string(), 200)
        } else {
            detail
        };
        return Err(GatewayError::with_status(
            502,
            format!("Cline 设备授权失败（{status}）: {detail}"),
        ));
    }
    let text_field = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let Some(device_code) = text_field("device_code") else {
        return Err(GatewayError::with_status(
            502,
            "Cline 设备授权响应缺少 device_code",
        ));
    };
    let Some(user_code) = text_field("user_code") else {
        return Err(GatewayError::with_status(
            502,
            "Cline 设备授权响应缺少 user_code",
        ));
    };
    let verification_uri = text_field("verification_uri")
        .unwrap_or_else(|| credentials::DEVICE_VERIFY_FALLBACK.to_string());
    Ok(DeviceAuthStart {
        device_code,
        user_code,
        verification_uri,
        verification_uri_complete: text_field("verification_uri_complete"),
        expires_in_seconds: payload
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(300)
            .min(MAX_POLL_MS / 1000),
        interval_seconds: payload
            .get("interval")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_INTERVAL_SECONDS)
            .max(1),
    })
}

/// `login.start`：发起设备授权，把「去哪儿输码」写进说明。
///
/// `flow` 用 `device_code`（后续 `login.poll` 只凭它继续轮询，插件不需要
/// 保存任何状态）。
pub async fn login_start(method: &str) -> Result<proto::LoginStart, GatewayError> {
    if !method.is_empty() && method != "browser" {
        return Err(GatewayError::with_status(
            400,
            format!("Cline 不支持这种登录方式：{method}"),
        ));
    }
    let start = start().await?;
    // 有 `verification_uri_complete` 时优先用它（码已预填，用户少一步）
    let url = start
        .verification_uri_complete
        .clone()
        .unwrap_or_else(|| start.verification_uri.clone());
    Ok(proto::LoginStart {
        flow: start.device_code.clone(),
        url,
        instructions: format!(
            "在打开的页面里输入授权码 {} 并确认（{} 秒内有效）；确认后网关会自动取回凭证",
            start.user_code, start.expires_in_seconds
        ),
        // 轮询型（> 0）：网关按这个间隔反复调 `login.poll`
        poll_interval_ms: start.interval_seconds * 1000,
        // 设备授权页在系统浏览器里打开更稳（WorkOS 的登录页会走第三方密码管理器）
        external_browser: true,
        allowed_hosts: vec![
            "authkit.cline.bot".to_string(),
            "api.workos.com".to_string(),
        ],
    })
}

/// `login.poll`：轮询一次。
///
/// 三档结果：还没确认 → `Pending`；确认了 → `Done`（含落库形态的账号）；
/// 失败 → `Failed`（附可读原因）。
pub async fn login_poll(device_code: &str) -> Result<proto::LoginPoll, GatewayError> {
    if device_code.trim().is_empty() {
        return Err(GatewayError::with_status(400, "Cline 登录流程 id 为空"));
    }
    let headers = vec![("Accept".to_string(), "application/json".to_string())];
    let body = format!(
        "grant_type=urn:ietf:params:oauth:grant-type:device_code&device_code={}&client_id={WORKOS_CLIENT_ID}",
        device_code
    );
    let response = form_post(
        &format!("{WORKOS_BASE_URL}/user_management/authenticate"),
        &body,
        &headers,
    )
    .await?;
    let status = response.0;
    let payload = response.1;
    if (200..300).contains(&status) {
        let access = payload
            .get("access_token")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty());
        let refresh = payload
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty());
        let (Some(access), Some(refresh)) = (access, refresh) else {
            return Ok(proto::LoginPoll::Failed {
                message: "Cline 登录响应缺少令牌（access_token / refresh_token）".to_string(),
            });
        };
        // 第四步：换成 Cline 自己的会话令牌
        let (session_access, session_refresh) = register_session(access, refresh).await?;
        return Ok(proto::LoginPoll::Done {
            account: account_of(&session_access, &session_refresh),
        });
    }
    // 非 2xx：按 RFC 8628 的 error 码决定继续还是终止
    let error_code = payload
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match error_code.as_str() {
        "authorization_pending" | "slow_down" => Ok(proto::LoginPoll::Pending {
            hint: Some("等待在浏览器里完成授权…".to_string()),
        }),
        "expired_token" => Ok(proto::LoginPoll::Failed {
            message: "Cline 登录超时（授权码已过期），请重新发起登录".to_string(),
        }),
        "access_denied" => Ok(proto::LoginPoll::Failed {
            message: "Cline 登录被拒绝（用户在授权页取消了）".to_string(),
        }),
        _ => {
            let detail = payload
                .get("error_description")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| truncate_text(&payload.to_string(), 200));
            Ok(proto::LoginPoll::Failed {
                message: format!("Cline 登录失败（{status}）: {detail}"),
            })
        }
    }
}

/// 发一次表单 POST（WorkOS 的两个端点都只认 form 编码）。
///
/// 返回 `(状态码, 解析后的 JSON)`；非 JSON 体给 `Value::Null`。
///
/// ── 为什么不用 `http::send_raw` ──────────────────────────────
/// 那个函数的 `body` 是 `Option<&Value>`，会按 JSON 发（并强制
/// `Content-Type: application/json`），而 WorkOS 只认
/// `application/x-www-form-urlencoded` —— 发错编码的表现是 400
/// `invalid_request`，与「码不对」难以区分。所以这里直发一次。
async fn form_post(
    url: &str,
    body: &str,
    extra_headers: &[(String, String)],
) -> Result<(u16, Value), GatewayError> {
    let client = reqwest::Client::builder()
        .use_rustls_tls()
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut request = client
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(body.to_string());
    for (key, value) in extra_headers {
        request = request.header(key.as_str(), value.as_str());
    }
    let response = request.send().await.map_err(|error| {
        GatewayError::with_status(502, format!("Cline 登录请求失败: {}", describe_chain(&error)))
    })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

/// 把 WorkOS 令牌换成 Cline 会话令牌（第四步，**不能省**）。
///
/// 返回 `(accessToken 含前缀, refreshToken)`。
async fn register_session(
    workos_access: &str,
    workos_refresh: &str,
) -> Result<(String, String), GatewayError> {
    let headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "application/json".to_string()),
        ("X-CLIENT-TYPE".to_string(), super::CLIENT_TYPE.to_string()),
    ];
    let body = json!({
        "accessToken": workos_access,
        "refreshToken": workos_refresh,
    });
    let response = send_raw(
        "POST",
        &format!("{}/auth/register", credentials::API_BASE_URL),
        Some(&body),
        &headers,
        None,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await
    .map_err(|error| {
        GatewayError::with_status(
            502,
            format!("Cline 令牌登记失败: {}", describe_chain(&error)),
        )
    })?;
    let status = response.status;
    let payload = response.payload.clone().unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let detail = payload
            .get("error")
            .and_then(|error| {
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .or_else(|| error.as_str())
            })
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| truncate_text(&payload.to_string(), 200));
        return Err(GatewayError::with_status(
            502,
            format!("Cline 令牌登记失败（{status}）: {detail}"),
        ));
    }
    let data = payload.get("data").unwrap_or(&payload);
    let access = data
        .get("accessToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| GatewayError::with_status(502, "Cline 令牌登记响应缺少 accessToken"))?;
    let refresh = data
        .get("refreshToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(workos_refresh)
        .to_string();
    Ok((credentials::ensure_token_prefix(access), refresh))
}

/// 会话令牌对 → `NormalizedAccount`（网关落库用）。
///
/// `secret` 的键名与 `credentials_from_record` 的读取侧一一对应
/// （`accessToken` / `refreshToken` / `expiresAt` / `account` / `displayName`），
/// `public` 给界面显示用的那几格。
pub(super) fn account_of(access: &str, refresh: &str) -> proto::NormalizedAccount {
    let (identity, _) = credentials::identity_from_jwt(access);
    let expires_at = credentials::expires_at_from_jwt(access);
    let account_id = credentials::account_id_from_jwt(access).unwrap_or_default();
    let name = if identity.is_empty() {
        account_id.clone()
    } else {
        identity.clone()
    };
    let tail: String = {
        let chars: Vec<char> = access.chars().collect();
        let start = chars.len().saturating_sub(4);
        chars[start..].iter().collect()
    };
    let mut secret = serde_json::Map::new();
    secret.insert("accessToken".to_string(), Value::String(access.to_string()));
    if !refresh.is_empty() {
        secret.insert(
            "refreshToken".to_string(),
            Value::String(refresh.to_string()),
        );
    }
    if let Some(expires) = expires_at {
        secret.insert("expiresAt".to_string(), json!(expires));
    }
    if !account_id.is_empty() {
        secret.insert("account".to_string(), Value::String(account_id.clone()));
    }
    if !name.is_empty() {
        secret.insert("displayName".to_string(), Value::String(name.clone()));
    }
    proto::NormalizedAccount {
        secret: Value::Object(secret),
        public: json!({
            "name": name,
            "uid": account_id,
            "tokenTail": tail,
            "expiresAt": expires_at,
        }),
        identity: if account_id.is_empty() { identity } else { account_id },
        desktop: false,
    }
}
