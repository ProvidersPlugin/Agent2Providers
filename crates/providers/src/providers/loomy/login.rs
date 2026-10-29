//! Loomy 手机号验证码登录（**依据客户端实现**：接口从桌面端 `app.asar` 的
//! `electron/xfyun/account-service.js` 读出并逐字核对）。
//!
//! ── 链路（两步，没有窗口、没有轮询）──────────────────────────
//! ```text
//! POST {xfyun}/login/phone/sendMsgCode   {base, param:{ccode, phone, expire:300}}
//!   → data.msgid（验证码会话 id）
//! POST {xfyun}/login/phone/checkCode     {base, param:{ccode, phone, mcode, msgid,
//!                                         expire:1209600}}   ← 14 天
//!   → data.session / data.userid（登录态）
//! ```
//! 两个请求都带 HMAC-SHA1 签名头（见 `sign.rs`）。`base` 信封的字段取自
//! 客户端 `_makeBase()`：`appid=GM3LOOMY`、`modelid="Web"`、`version="1.0.0"`、
//! `devid="web"`、`ua`、`traceid`（随机十六进制）。
//!
//! ── 为什么这条链路不进 `login` 的通用形态 ────────────────────
//! 协议里的 `LoginStart` 只有「开窗口 / 轮询」两种形态，而本家是**两次同步
//! 调用**：发码 → 用户填码 → 登录。网关那边对应的是两个 HTTP 端点
//! （`/api/session/login/loomy/sms/{send,verify}`），它们直接调本模块的两个
//! 函数，而不是走 `login.start`。因此这里不实现 `Provider::login_*`。
//!
//! ── msgid 由前端带回 ───────────────────────────────────────
//! 上游把「刚发的码」绑在发码响应的 msgid 上，登录必须带同一个。插件不替用户
//! 保存这个中间态（一次登录可以跨多次 HTTP 请求、也可以被放弃），回给前端让它
//! 随下一次请求带回是这里最省事又不丢正确性的做法。

use serde_json::{json, Value};

use super::client;
use super::credentials;

/// 发码路径（客户端 `sendSmsCode`）
const SEND_CODE_PATH: &str = "/login/phone/sendMsgCode";
/// 验证码登录路径（客户端 `loginBySmsCode`）
const LOGIN_PATH: &str = "/login/phone/checkCode";

/// 客户端 `_makeBase()` 的常量：modelid / devid / ua / version
const WEB_MODEL_ID: &str = "Web";
const WEB_DEVICE_ID: &str = "web";
const CLIENT_UA: &str = "Loomy|Desktop|Electron|macOS";
const CLIENT_VERSION: &str = "1.0.0";

/// 中国大陆手机号（与客户端 `sendSmsCode` 的 `^1[3-9]\d{9}$` 同口径，
/// 容忍 `+86` / `86` 前缀与空格、连字符）
pub fn normalize_phone(raw: &str) -> Result<String, String> {
    let trimmed: String = raw
        .chars()
        .filter(|ch| !ch.is_whitespace() && *ch != '-')
        .collect();
    let digits = trimmed.strip_prefix("+86").unwrap_or(&trimmed);
    let digits = digits
        .strip_prefix("86")
        .filter(|rest| rest.len() == 11)
        .unwrap_or(digits);
    if digits.len() == 11
        && digits.starts_with('1')
        && digits
            .chars()
            .nth(1)
            .is_some_and(|ch| ('3'..='9').contains(&ch))
        && digits.chars().all(|ch| ch.is_ascii_digit())
    {
        Ok(digits.to_string())
    } else {
        Err("请填写 11 位中国大陆手机号".to_string())
    }
}

/// 6 位数字验证码（客户端把 `mcode` 当字符串传；这里保持字符串形态）
fn normalize_code(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.len() == 6 && trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        Ok(trimmed.to_string())
    } else {
        Err("请填写 6 位数字验证码".to_string())
    }
}

/// 请求信封 `base`（客户端 `_makeBase()`：traceid 是随机十六进制）
fn make_base() -> Value {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0);
        for (index, slot) in bytes.iter_mut().enumerate() {
            *slot = (nanos >> ((index % 8) * 8)) as u8;
        }
    }
    let traceid: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    json!({
        "appid": super::endpoints::app_id(),
        "modelid": WEB_MODEL_ID,
        "version": CLIENT_VERSION,
        "devid": WEB_DEVICE_ID,
        "ua": CLIENT_UA,
        "traceid": traceid,
    })
}

/// 业务码 → 用户能看懂的原因（未知码回落到上游文案）。
fn describe_login_error(code: &str, upstream_msg: &str) -> String {
    match code {
        "020002" | "100002" => "登录态已失效，请重新登录".to_string(),
        _ => {
            if upstream_msg.trim().is_empty() {
                format!("登录失败（上游 code={code}）")
            } else {
                format!("登录失败：{}", upstream_msg.trim())
            }
        }
    }
}

/// 从发码响应里取 msgid（上游字段名在不同版本里出现过 msgid / msgId 两种形态，
/// 且可能落在 `data` 或 `data.data` 两层 —— 这里按候选路径逐个试）。
fn extract_msgid(payload: &Value) -> String {
    let candidates = [
        payload.pointer("/data/msgid"),
        payload.pointer("/data/msgId"),
        payload.pointer("/data/data/msgid"),
        payload.pointer("/data/data/msgId"),
    ];
    for value in candidates {
        let text = value.and_then(Value::as_str).unwrap_or("").trim();
        if !text.is_empty() {
            return text.to_string();
        }
    }
    String::new()
}

/// 发送短信验证码。返回 `{msgid}`（前端随登录请求带回）。
pub async fn send_code(proxy: &str, phone: &str) -> Result<Value, String> {
    let phone = normalize_phone(phone)?;
    let client = crate::http::control_client(None, proxy);
    let body = json!({
        "base": make_base(),
        "param": { "ccode": "86", "phone": phone, "expire": 300 },
    });
    let payload = client::signed_post(&client, SEND_CODE_PATH, &body, "发送验证码").await?;
    let code = client::business_code(&payload);
    if code != client::SUCCESS_CODE {
        let message = describe_login_error(&code, &client::upstream_message(&payload));
        agent2api_provider_sdk::log_warn!(
            "[Loomy] 验证码发送失败（{code}）: {message}"
        );
        return Err(message);
    }
    let msgid = extract_msgid(&payload);
    if msgid.is_empty() {
        agent2api_provider_sdk::log_warn!("[Loomy] 验证码已发出但上游未返回 msgid");
        return Err("验证码已发出但上游没有返回 msgid，请稍后重试".to_string());
    }
    // 不打手机号（它是个人信息）：只留「发给了尾号」的可核对痕迹
    agent2api_provider_sdk::log_info!(
        "[Loomy] 📱 验证码已发送（{}）",
        credentials::mask_phone(&phone)
    );
    Ok(json!({ "msgid": msgid }))
}

/// 用手机号 + 验证码登录，返回一份 `NormalizedAccount`
/// （`{secret, public, identity, desktop}`）。
pub async fn login_with_code(
    proxy: &str,
    phone: &str,
    code: &str,
    msgid: Option<&str>,
    name: Option<&str>,
) -> Result<agent2api_provider_protocol::NormalizedAccount, String> {
    let phone = normalize_phone(phone)?;
    let code = normalize_code(code)?;
    let msgid = msgid.map(str::trim).filter(|value| !value.is_empty());
    let Some(msgid) = msgid else {
        return Err(
            "缺少 msgid：请先点「发送验证码」，用本次发码返回的 msgid 登录".to_string(),
        );
    };
    let client = crate::http::control_client(None, proxy);
    let body = json!({
        "base": make_base(),
        "param": {
            "ccode": "86",
            "phone": phone,
            "mcode": code,
            "msgid": msgid,
            // 会话 14 天（客户端 loginBySmsCode 的同款取值）
            "expire": 14 * 24 * 3600,
        },
    });
    let payload = client::signed_post(&client, LOGIN_PATH, &body, "登录").await?;
    let status = client::business_code(&payload);
    if status != client::SUCCESS_CODE {
        let upstream_msg = client::upstream_message(&payload);
        let message = describe_login_error(&status, &upstream_msg);
        agent2api_provider_sdk::log_warn!(
            "[Loomy] 验证码登录失败（code={status}，上游原文「{upstream_msg}」）: {message}"
        );
        return Err(message);
    }
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    let session = data
        .get("session")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if session.is_empty() {
        agent2api_provider_sdk::log_warn!("[Loomy] 登录成功但上游未返回 session");
        return Err("登录成功但上游没有返回 session".to_string());
    }
    let userid = data
        .get("userid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    agent2api_provider_sdk::log_info!(
        "[Loomy] ✅ 登录成功（{}，userid {}）",
        credentials::mask_phone(&phone),
        if userid.is_empty() { "缺失" } else { &userid }
    );
    let now = crate::http::now_ms();
    let display = name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| credentials::default_name(&phone, &userid));
    Ok(agent2api_provider_protocol::NormalizedAccount {
        secret: json!({
            "session": session,
            "userId": userid,
            "phone": phone,
            // 不写 `expiresAt`：上游不给到期时刻，估算值不是事实（见
            // `credentials` 模块头）。失效只认上游的 020002 / 100002。
            "addedAt": now,
        }),
        public: json!({
            "name": display,
            "uid": userid,
            "phone": phone,
            "tokenTail": credentials::session_tail(&session),
        }),
        identity: if userid.is_empty() {
            format!("loomy-{phone}")
        } else {
            format!("loomy-{userid}")
        },
        desktop: false,
    })
}
