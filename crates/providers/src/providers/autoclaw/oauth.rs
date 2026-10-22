//! AutoClaw **国际版**的 OAuth 网页登录（Zai / Google）。
//!
//! ── 协议（三步，实测）───────────────────────────────────────
//! ```text
//! ① POST {intl}/userapi/overseasv1/oauth-captcha-config   {}
//!      → data {enabled, region, prefix, scene_id, captcha_supplier}
//!      ★ 这就是「这个构建要不要走 OAuth」的开关：国内版返回 enabled:false。
//! ② POST {intl}/userapi/overseasv1/{zai|google}-oauth-url
//!      {source_id, device_id, navigate_uri, ali_captcha_verify_param}
//!      → data.oauth_url（官方登录页）
//! ③ 浏览器在登录页完成登录 → 302 到 {navigate_uri}?code=…&state=…
//!    POST {intl}/userapi/overseasv1/{zai|google}-oauth-login
//!      {source_id, device_id, code, state, navigate_uri}
//!      → data {access_token, refresh_token, user_id, user_name, first_login}
//! ```
//!
//! ── 卡点：第 ② 步强制要求阿里云风控验证码 ────────────────────
//! 不带 `ali_captcha_verify_param` 得到 `631002 当前版本已停止服务`（一个与真实
//! 原因无关的误导码）；带假值得到 `630014 审核失败`。验证码是**浏览器端 JS SDK**，
//! 复刻算法成本高且会随上游更新失效 —— 因此不复刻，而是让插件**接收**前端跑完
//! SDK 得到的那个不透明字符串并原样转交。第 ③ 步不需要验证码（用假 code 打
//! `zai-oauth-login` 得到 `631001 User login error` 而不是 630014），所以换码
//! 可以完全在插件里做。
//!
//! ── navigate_uri 必须长成客户端那样（别改）─────────────────────
//! Zai 的 OAuth 服务按 client 登记的白名单逐字校验 `redirect_uri`：名单里只有
//! 官方客户端那四个 loopback 端口上的 `http://localhost:<端口>/auth/callback-zai`。
//! 端口不是那四个之一、host 写成 `127.0.0.1`、路径不对 —— 一律被拒，窗口里直接
//! 渲染 `{"detail":"Redirect URI not registered for this client"}`。
//! **Google 是另一套规则**（RFC 8252 的 loopback 宽松匹配，端口任意），所以同一
//! 份代码「谷歌能登、Zai 登不上」—— 端口那一半由 [`callback_server`] 解决。
//!
//! ── 与网关版的差别：回调落在**插件自己的**监听器上 ─────────────
//! 网关版把手里的 `navigate_uri` 指向网关端口，再靠 `callback_server` 借端口
//! 302 转发回网关。插件版没有「另一个进程的回调路由」可转 —— 因此监听器自己
//! 收下 code/state、自己换码、把结果放进流程表，网关按 `poll_interval_ms` 来问。
//!
//! ── 安全 ────────────────────────────────────────────────────
//! 回调落在本机 HTTP 端口上，任何本机进程都能发出一次 GET。安全由**一次性
//! 授权码**与流程表承担：code 一次性、流程必须进行中、换码失败流程即失败。
//! token 只进账号库，不进日志。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 本 crate 编译期禁止 unwrap/expect/panic。

use serde_json::{json, Value};

use crate::errors::GatewayError;
use crate::http::send_raw;
use crate::logging;

use super::credentials::{self, new_device_id};
use super::refresh::signed_auth_headers;
use super::region::Region;

/// 验证码配置（同时是「这个构建要不要走 OAuth」的开关）
const CAPTCHA_CONFIG_PATH: &str = "/userapi/overseasv1/oauth-captcha-config";

/// 请求超时：与客户端同档（`OVERSEA_OAUTH_*_TIMEOUT_MS` = 15 秒）。
const REQUEST_TIMEOUT_MS: u64 = 15_000;

/// OAuth 变体（上游按变体分两个端点，字段与流程完全一致）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Vendor {
    /// Zai（智谱自家账号体系）
    Zai,
    /// Google
    Google,
}

impl Vendor {
    /// 两个变体（顺序 = 界面上按钮的顺序：Zai 在前，与客户端一致）
    pub(super) const ALL: [Vendor; 2] = [Vendor::Zai, Vendor::Google];

    /// 请求体 / 回调路径里用的标识（`zai` / `google`）
    pub(super) fn id(self) -> &'static str {
        match self {
            Self::Zai => "zai",
            Self::Google => "google",
        }
    }

    /// 解析回调路径里的标识（不认识返回 None —— 不静默回落到某一个变体）
    pub(super) fn from_id(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|vendor| vendor.id() == value)
    }

    /// 界面上那个按钮的名字
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Zai => "Zai",
            Self::Google => "Google",
        }
    }

    /// 取授权地址的端点
    fn url_path(self) -> &'static str {
        match self {
            Self::Zai => "/userapi/overseasv1/zai-oauth-url",
            Self::Google => "/userapi/overseasv1/google-oauth-url",
        }
    }

    /// 用授权码换凭证的端点
    fn login_path(self) -> &'static str {
        match self {
            Self::Zai => "/userapi/overseasv1/zai-oauth-login",
            Self::Google => "/userapi/overseasv1/google-oauth-login",
        }
    }
}

/// 回调路径的**前缀**（与官方客户端逐字同款：客户端 `getZaiCallbackUri()` 给的
/// 就是 `http://localhost:<port>/auth/callback-zai|google`）。
pub(super) const CALLBACK_PATH_PREFIX: &str = "/auth/callback-";

/// 拼这次登录的回调地址（同时也是交给上游的 `navigate_uri`）。
pub(super) fn navigate_uri(callback_base: &str, vendor: Vendor) -> String {
    format!(
        "{}{}{}",
        callback_base.trim_end_matches('/'),
        CALLBACK_PATH_PREFIX,
        vendor.id()
    )
}

/// 带签名的 userapi POST（登录链路上还没有 token，因此走匿名形态）。
async fn post_signed(
    region: Region,
    path: &str,
    body: &Value,
    what: &str,
) -> Result<Value, GatewayError> {
    let url = format!("{}{path}", credentials::userapi_base_url(region));
    let headers = signed_auth_headers("");
    let response = send_raw(
        "POST",
        &url,
        Some(body),
        &headers,
        None,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await
    .map_err(|error| {
        if error.is_timeout() {
            GatewayError::with_status(504, format!("{what}超时"))
        } else {
            GatewayError::with_status(502, format!("{what}失败: {error}"))
        }
    })?;
    if !response.ok {
        return Err(GatewayError::with_status(
            response.status as i32,
            format!("{what}返回 HTTP {}", response.status),
        ));
    }
    Ok(response.payload.unwrap_or(Value::Null))
}

/// 业务码 → 用户能看懂的原因（OAuth 这条链自己的码表）。
fn describe_oauth_error(code: i64, upstream_msg: &str) -> String {
    match code {
        631_002 => {
            "本次登录缺少风控验证（验证码未通过或已过期），请重新点击登录再试一次".to_string()
        }
        630_014 => "风控验证未通过，请重新点击登录再试一次".to_string(),
        631_001 => "授权码无效或已过期，请重新点击登录".to_string(),
        400_001 => "请求参数有误（登录链路内部错误，请重试）".to_string(),
        400_002 => "请求签名校验失败（本机时钟可能有偏差），请校准系统时间后重试".to_string(),
        _ => {
            if upstream_msg.trim().is_empty() {
                format!("登录失败（上游 code={code}）")
            } else {
                format!("登录失败：{}", upstream_msg.trim())
            }
        }
    }
}

/// 取验证码配置（前端据此初始化阿里云 SDK）。
///
/// 返回归一化形状 `{enabled, region, prefix, sceneId, supplier}`：上游用
/// snake_case，前端 camelCase，在**这一处**翻译完。`enabled: false` **不是错误**
/// —— 国内版就是这个值（它没有 OAuth）。
pub(super) async fn captcha_config(region: Region) -> Result<Value, GatewayError> {
    let payload = post_signed(region, CAPTCHA_CONFIG_PATH, &json!({}), "获取风控验证配置").await?;
    let code = payload.get("code").and_then(Value::as_i64).unwrap_or(0);
    if code != 0 {
        let upstream_msg = payload.get("msg").and_then(Value::as_str).unwrap_or("");
        return Err(GatewayError::with_status(
            502,
            describe_oauth_error(code, upstream_msg),
        ));
    }
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    let text = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    Ok(json!({
        "enabled": data.get("enabled").and_then(Value::as_bool).unwrap_or(false),
        "region": text("region"),
        "prefix": text("prefix"),
        "sceneId": text("scene_id"),
        "supplier": text("captcha_supplier"),
    }))
}

/// 取 OAuth 授权地址（**需要验证码参数**）。
pub(super) async fn request_oauth_url(
    region: Region,
    vendor: Vendor,
    navigate_uri: &str,
    captcha_verify_param: &str,
    device_id: &str,
) -> Result<String, GatewayError> {
    let captcha = captcha_verify_param.trim();
    if captcha.is_empty() {
        // 本地就挡掉：不带它打上游必然得到那条误导性的 631002
        return Err(GatewayError::with_status(
            400,
            "缺少风控验证参数，请先完成验证码",
        ));
    }
    let body = json!({
        "source_id": "autoclaw",
        "device_id": device_id,
        "navigate_uri": navigate_uri,
        "ali_captcha_verify_param": captcha,
    });
    let what = format!("获取 {} 授权地址", vendor.label());
    let payload = post_signed(region, vendor.url_path(), &body, &what).await?;
    let code = payload.get("code").and_then(Value::as_i64).unwrap_or(0);
    if code != 0 {
        let upstream_msg = payload.get("msg").and_then(Value::as_str).unwrap_or("");
        let message = describe_oauth_error(code, upstream_msg);
        logging::log(
            "[Login]",
            &format!(
                "AutoClaw {} 授权地址获取失败（code={code}，上游原文「{upstream_msg}」）: {message}",
                vendor.label()
            ),
        );
        return Err(GatewayError::with_status(400, message));
    }
    let oauth_url = payload
        .get("data")
        .and_then(|data| data.get("oauth_url"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if oauth_url.is_empty() {
        return Err(GatewayError::with_status(
            502,
            "上游没有返回授权地址，请重试",
        ));
    }
    Ok(oauth_url)
}

/// 用回调里的一次性授权码换凭证，返回可落库的扁平凭证（含 user_name）。
pub(super) async fn exchange_code(
    region: Region,
    vendor: Vendor,
    code: &str,
    state: &str,
    navigate_uri: &str,
    device_id: &str,
) -> Result<Value, GatewayError> {
    let body = json!({
        "source_id": "autoclaw",
        "device_id": device_id,
        "code": code,
        "state": state,
        "navigate_uri": navigate_uri,
    });
    let what = format!("{} 登录换取凭证", vendor.label());
    let payload = post_signed(region, vendor.login_path(), &body, &what).await?;
    let status = payload.get("code").and_then(Value::as_i64).unwrap_or(0);
    if status != 0 {
        let upstream_msg = payload.get("msg").and_then(Value::as_str).unwrap_or("");
        let message = describe_oauth_error(status, upstream_msg);
        logging::log(
            "[Login]",
            &format!(
                "AutoClaw {} 换取凭证失败（code={status}，上游原文「{upstream_msg}」）: {message}",
                vendor.label()
            ),
        );
        return Err(GatewayError::with_status(400, message));
    }
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    let token = data
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if token.is_empty() {
        logging::log(
            "[Login]",
            &format!(
                "AutoClaw {} 登录成功但上游未返回 access_token",
                vendor.label()
            ),
        );
        return Err(GatewayError::with_status(
            502,
            "登录成功但上游没有返回 access_token",
        ));
    }
    let refresh_token = data
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let user_name = data
        .get("user_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let mut credentials = json!({ "token": token, "deviceId": device_id });
    if let Some(object) = credentials.as_object_mut() {
        if !refresh_token.is_empty() {
            object.insert("refreshToken".to_string(), Value::String(refresh_token));
        }
        // 备注名兜底：OAuth 账号有用户名（手机验证码那条没有，用脱敏手机号）
        if !user_name.is_empty() {
            object.insert("userName".to_string(), Value::String(user_name));
        }
    }
    Ok(credentials)
}

/// 生成一个设备 id（与短信那条同一形态：32 字节随机 → 64 位十六进制）。
///
/// 转发一层而不是让调用方各自 import：两跳（`oauth-url` / `oauth-login`）必须
/// 带同一个值，形态一旦需要变化（比如上游要求 ed25519 指纹），改这里一处即可。
pub(super) fn new_oauth_device_id() -> String {
    new_device_id()
}
