//! AutoClaw 手机号验证码登录（**国内版唯一官方登录方式**）。
//!
//! ── 两个地区的登录方式不一样，而且各只有一条 ────────────────
//! ```text
//!                        国内版（autoclaw）      国际版（autoclaw-intl）
//!   手机验证码登录        ✅ 唯一官方方式          ❌ 不提供（见下）
//!   OAuth 网页登录        ❌ 登录页不渲染按钮      ✅ 唯一登录方式（Zai / Google）
//!   oauth-captcha-config  enabled: false           enabled: true, supplier: aliyun
//! ```
//!
//! 国际版的手机验证码入口**曾经可用**（接口逐字相同、实测也通），移除它是产品
//! 决策而不是技术限制：国际版用户大量用 Zai / Google 注册、根本没绑手机号，
//! 留着那个入口只会稳定失败。所以这里对国际版直接返回人话错误 —— 入口只是入口，
//! 老界面 / 脚本 / 直接打 HTTP 的调用方仍可能带着 `autoclaw-intl` 进来。
//!
//! ── 为什么这条链是两次同步调用，而不是 `login.start` ──────────
//! 官方客户端唯一的自助入口就是「发码 → 填码 → 换 token」，没有授权码、
//! 没有窗口、没有轮询。硬套网页登录框架会让网关为一次短信建一个后台任务、
//! 还要把 deviceId 编进 flow 再拆出来。因此这里走**管理动作**：
//!
//! ```text
//!   login.sms-send    {phone, name?}                 → {deviceId}
//!   login.sms-verify  {phone, code, deviceId, name?} → NormalizedAccount（网关落库）
//! ```
//!
//! ── 上游协议（依据桌面端实现 `app.asar`，实测确认）───────────────
//! ```text
//! POST {userapi}/userapi/v1/agent-send-code   {"phone","source_id","device_id"}
//! POST {userapi}/userapi/v1/agent-login/      {"phone","code","platform","source_id","device_id"}
//! ```
//! `agent-login` **直接返回 token 对**（不经授权码）。`platform: "web"` 是网页端
//! 与桌面端唯一的差别（源实现 `loginWebWithPhoneCode`）。签名头与积分 / 签到
//! 链路完全一致（`X-Auth-Sign = MD5(appId&ts&appKey)`，两地密钥逐字相同），
//! 因此复用 [`super::refresh::signed_auth_headers`]。
//!
//! ── deviceId 从哪来 ─────────────────────────────────────────
//! 官方客户端是「ed25519 公钥的 SHA-256」，但服务端只把它当一个不透明的 64 位
//! 十六进制串。这里生成密码学随机的 32 字节十六进制串即可 —— 它承担的全部职责
//! 就是「区分不同设备」，且是 `agent-send-code` / `zai-oauth-url` 的必填项
//! （不带会得到误导性的 `400001 请求数据有问题`）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 本 crate 编译期禁止 unwrap/expect/panic（见 `main.rs` 的 deny）。

use agent2api_provider_protocol as proto;
use serde_json::{json, Value};

use crate::errors::GatewayError;
use crate::http::send_raw;
use crate::logging;

use super::credentials;
use super::refresh::signed_auth_headers;
use super::region::Region;

/// 发验证码路径（源实现 `sendCode`）
const SEND_CODE_PATH: &str = "/userapi/v1/agent-send-code";
/// 登录路径（源实现 `phoneCodeLogin`，注意结尾的斜杠是上游要求的）
const LOGIN_PATH: &str = "/userapi/v1/agent-login/";

/// 请求超时（验证码接口偶发慢，给到 20 秒）
const REQUEST_TIMEOUT_MS: u64 = 20_000;

/// 这条链路只服务国内版。
fn ensure_sms_region(region: Region) -> Result<(), GatewayError> {
    if region == Region::Cn {
        return Ok(());
    }
    Err(GatewayError::with_status(
        400,
        "AutoClaw 国际版不支持手机号登录，请改用「网页登录（Zai / Google）」或「填写凭证」",
    ))
}

/// 中国大陆手机号（源实现 `normalizePhone` 的等价物：`1[2-9]` 开头的 11 位，
/// 容忍 `+86` / `86` 前缀与空格、连字符）。
pub(super) fn normalize_phone(raw: &str) -> Result<String, GatewayError> {
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
            .is_some_and(|ch| ('2'..='9').contains(&ch))
        && digits.chars().all(|ch| ch.is_ascii_digit())
    {
        Ok(digits.to_string())
    } else {
        Err(GatewayError::with_status(400, "请填写 11 位中国大陆手机号"))
    }
}

/// 6 位数字验证码（源实现 `normalizeCode`）。
///
/// ── 为什么返回**数字**而不是字符串（实测踩过的坑，别改回去）──────
/// 上游要求 body 里的 `code` 是 **JSON 数字**，传字符串会得到
/// `400001 / 请求数据有问题` —— 一个与「验证码错误」完全无关的参数错误。
fn normalize_code(raw: &str) -> Option<u32> {
    let trimmed = raw.trim();
    if trimmed.len() == 6 && trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        trimmed.parse::<u32>().ok()
    } else {
        None
    }
}

/// 发一次带签名的 userapi POST（登录链路上还没有 token，走匿名形态）。
async fn post_unauthenticated(
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

/// 业务码 → 用户能看懂的原因。
///
/// 上游的参数类错误一律给 `400001 / 请求数据有问题`，对用户没有指导意义、
/// 也分不清是哪个字段不对。这里按码翻成「该做什么」，未知码才回落到上游文案。
fn describe_login_error(code: i64, upstream_msg: &str) -> String {
    match code {
        400_001 => "请求参数有误，请检查手机号与验证码格式后重试".to_string(),
        630_202 => "验证码不正确，请检查后重试".to_string(),
        630_101 => "获取验证码过于频繁，请稍后再试".to_string(),
        410_000 | 400_000 => "登录态已失效，请重新登录".to_string(),
        400_002 => "请求签名校验失败（本机时钟可能有偏差），请校准系统时间后重试".to_string(),
        // 631002 原文是「当前版本已停止服务」—— 它不是版本问题，而是**缺少
        // 风控验证**（实测换任何 X-Version 都是这个码）。直译会把用户引向
        // 「升级客户端」这个死方向。
        631_002 => {
            "上游要求过风控验证，本次登录无法在此完成；\
             请改用「填写凭证」（可从 AutoClaw 客户端登录态文件里取）"
                .to_string()
        }
        630_014 => "风控验证未通过，请稍后重试；若持续失败请改用「填写凭证」".to_string(),
        _ => {
            if upstream_msg.trim().is_empty() {
                format!("登录失败（上游 code={code}）")
            } else {
                format!("登录失败：{}", upstream_msg.trim())
            }
        }
    }
}

/// 发送短信验证码（国内版专用）。
///
/// 返回 `{deviceId}`：上游把「刚发的码」绑在发码时的 device_id 上，登录必须
/// 带同一个。插件不替用户保存这个中间态（一次登录可跨多次 HTTP 请求、也可被
/// 放弃），回给前端让它随下一次请求带回。
pub(super) async fn send_code(region: Region, phone: &str) -> Result<Value, GatewayError> {
    ensure_sms_region(region)?;
    let phone = normalize_phone(phone)?;
    let device_id = credentials::new_device_id();
    let body = json!({
        "phone": phone,
        "source_id": "autoclaw",
        "device_id": device_id,
    });
    let payload = post_unauthenticated(region, SEND_CODE_PATH, &body, "发送验证码").await?;
    let code = payload.get("code").and_then(Value::as_i64).unwrap_or(0);
    if code != 0 {
        let upstream_msg = payload.get("msg").and_then(Value::as_str).unwrap_or("");
        let message = describe_login_error(code, upstream_msg);
        logging::log(
            "[Login]",
            &format!("AutoClaw 验证码发送失败（{code}）: {message}"),
        );
        return Err(GatewayError::with_status(400, message));
    }
    let delivered = payload
        .get("data")
        .and_then(|data| data.get("result"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !delivered {
        logging::log("[Login]", "AutoClaw 验证码发送失败：上游返回 result=false");
        return Err(GatewayError::with_status(502, "验证码发送失败，请稍后重试"));
    }
    // 不打手机号（它是个人信息）：只留下「发给了尾号 xxxx」这种可核对的痕迹
    logging::log(
        "[Login]",
        &format!("AutoClaw 验证码已发送（{}）", mask_phone(&phone)),
    );
    Ok(json!({ "deviceId": device_id }))
}

/// 用手机号 + 验证码登录，返回可直接落库的账号。
///
/// 返回形状与「粘贴 token 添加」的 `account.normalize` 产出**同形** —— 登录只是
/// 另一种拿到凭证的方式，落盘、命名、去重、优先级分配全部复用网关既有的落库
/// 通路，不新开一条写账号的路。
pub(super) async fn login_with_code(
    region: Region,
    phone: &str,
    code: &str,
    device_id: Option<&str>,
    name: Option<&str>,
) -> Result<proto::NormalizedAccount, GatewayError> {
    ensure_sms_region(region)?;
    let phone = normalize_phone(phone)?;
    let Some(code) = normalize_code(code) else {
        return Err(GatewayError::with_status(400, "请填写 6 位数字验证码"));
    };
    let device_id = match device_id.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => value.to_string(),
        None => credentials::new_device_id(),
    };
    let body = json!({
        "phone": phone,
        "code": code,
        // 网页端与桌面端**唯一**的差别（源实现 `loginWebWithPhoneCode`）
        "platform": "web",
        "source_id": "autoclaw",
        "device_id": device_id,
    });
    let payload = post_unauthenticated(region, LOGIN_PATH, &body, "登录").await?;
    let status = payload.get("code").and_then(Value::as_i64).unwrap_or(0);
    if status != 0 {
        let upstream_msg = payload.get("msg").and_then(Value::as_str).unwrap_or("");
        let message = describe_login_error(status, upstream_msg);
        logging::log(
            "[Login]",
            &format!(
                "AutoClaw 验证码登录失败（code={status}，上游原文「{upstream_msg}」）: {message}"
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
        logging::log("[Login]", "AutoClaw 登录成功但上游未返回 access_token");
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
    Ok(account_from_credentials(
        region,
        json!({
            "token": token,
            "refreshToken": refresh_token,
            "deviceId": device_id,
            "phoneTail": mask_phone(&phone),
            "name": name.map(str::trim).filter(|value| !value.is_empty()).unwrap_or(""),
        }),
    )?)
}

/// 把一份「扁平凭证」变成协议账号（解析 JWT 拿 user_id，再交给公开形态）。
pub(super) fn account_from_credentials(
    region: Region,
    mut secret: Value,
) -> Result<proto::NormalizedAccount, GatewayError> {
    let parsed = credentials::credentials_from_record(&secret, region)?;
    if parsed.token.trim().is_empty() {
        return Err(GatewayError::with_status(400, "AutoClaw token 为空"));
    }
    if let Some(object) = secret.as_object_mut() {
        if !parsed.user_id.is_empty() {
            object.insert("userId".to_string(), Value::String(parsed.user_id.clone()));
        }
        if let Some(expires_at) = parsed.expires_at {
            object.insert(
                "tokenExpiresAt".to_string(),
                crate::json::json_number(expires_at),
            );
        }
        // 空备注名不留键：留一个空串会让公开形态把它当成「用户指定的名字」
        if object
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|value| value.trim().is_empty())
        {
            object.remove("name");
        }
    }
    Ok(proto::NormalizedAccount {
        public: super::account_view(region, &secret),
        identity: super::identity_of(region, &secret),
        secret,
        desktop: false,
    })
}

/// 手机号脱敏（源实现 `maskPhone`：保留前 3 后 4）。
///
/// 按 `char` 切而非按字节：入参经 `normalize_phone` 归一化后当前是 11 位 ASCII
/// 数字，但同款实现已在 Loomy 一侧被请求体里的汉字打穿过 —— release 是
/// `panic = "unwind"`，这类切片错一次就是整个插件进程退出。
fn mask_phone(phone: &str) -> String {
    let chars: Vec<char> = phone.chars().collect();
    if chars.len() < 7 {
        return phone.to_string();
    }
    let head: String = chars[..3].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}****{tail}")
}
