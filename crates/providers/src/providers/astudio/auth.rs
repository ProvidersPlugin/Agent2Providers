//! AStudio 的账号登录协议（从桌面端 `apps/server/dist/index.mjs` 逆向）。
//!
//! ── 上游与接口 ──────────────────────────────────────────────
//! ```text
//!   账号服务  https://agent.xfyun.cn/xingchen-studio/
//!   GET  chat/gee-captcha                 极验配置 {gt, challenge, success, new_captcha}
//!   POST login/mobile/send-verify-code    发短信验证码（multipart，含极验三件）
//!   POST login/phone-quick-login          手机号 + 验证码登录（不需要极验）
//!   POST login/check-account              账号 + 密码登录（需要极验）
//!   POST tenant-app/v2/init-app           Cookie → {banned}
//!   GET  userInfo                         Cookie → {userInfo:{uid, mobile}}
//!   GET  bot/models/configs               Cookie → [{api_key,is_current,is_default}]
//! ```
//!
//! ── 客户端身份头 ────────────────────────────────────────────
//! 桌面端每个请求都带 `clientType`（Windows 21 / macOS 22）与
//! `studioVersion`（安装包版本）。少了这两个头上游会按「未知客户端」处理，
//! 所以这里照发。
//!
//! ── 登录成功后拿到什么 ──────────────────────────────────────
//! `account_id` / `token`（或 ssoSessionId）/ `uid` / `mobile`，以及从模型
//! 配置里取出的 `modelBearerToken` —— 后者就是我们在「读取桌面端登录态」
//! 那条路里用的同一个凭据，所以登录完成后对话、余额、签到都能直接用。
//!
//! ── 极验 ────────────────────────────────────────────────────
//! 发短信与密码登录都要极验的 `geetest_challenge/validate/seccode`（v3，
//! 其中 seccode 以 base64 提交）。极验必须在浏览器里完成，所以插件起一个
//! 本机登录页（见 `login_page.rs`），页面里加载极验脚本并把结果交给这里。

use agent2api_provider_protocol as proto;
use reqwest::multipart::Form;
use serde_json::{json, Value};

use crate::http;

/// 账号服务根（生产环境）。
pub const AUTH_BASE: &str = "https://agent.xfyun.cn/xingchen-studio/";

/// 桌面端版本（安装包 `package.json` 的 version，用于 `studioVersion` 头）。
const STUDIO_VERSION: &str = "3.5.1";

/// 极验三件（页面完成滑块后拿到的）。
#[derive(Clone, Debug, Default)]
pub struct Captcha {
    pub challenge: String,
    pub validate: String,
    pub seccode: String,
}

impl Captcha {
    pub fn is_complete(&self) -> bool {
        !self.challenge.is_empty() && !self.validate.is_empty() && !self.seccode.is_empty()
    }
}

/// 一次成功登录拿到的会话。
#[derive(Clone, Debug, Default)]
pub struct Session {
    pub account_id: String,
    pub uid: String,
    pub token: String,
    pub sso_session_id: String,
    pub mobile: String,
    pub nickname: String,
    pub model_bearer_token: String,
    pub banned: bool,
}

impl Session {
    pub fn cookie_header(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.sso_session_id.is_empty() {
            parts.push(format!("ssoSessionId={}", self.sso_session_id));
            parts.push(format!("sso_sessionid={}", self.sso_session_id));
        }
        if !self.account_id.is_empty() {
            parts.push(format!("account_id={}", self.account_id));
        }
        if !self.token.is_empty() {
            parts.push(format!("token={}", self.token));
        }
        parts.join("; ")
    }
}

fn client_type() -> Option<&'static str> {
    if cfg!(target_os = "windows") {
        Some("21")
    } else if cfg!(target_os = "macos") {
        Some("22")
    } else {
        None
    }
}

fn client(proxy: Option<&proto::WireProxy>, gateway_proxy: &str) -> reqwest::Client {
    http::control_client(proxy, gateway_proxy)
}

/// 给请求加上桌面端身份头。
fn decorate(mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    if let Some(kind) = client_type() {
        request = request.header("clientType", kind);
    }
    request = request.header("studioVersion", STUDIO_VERSION);
    request
}

fn message_of(value: &Value) -> String {
    for key in ["message", "msg", "desc", "error"] {
        if let Some(text) = value.get(key).and_then(Value::as_str) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    String::new()
}

/// 拆信封：`{code:0, data:…}`；`code` 非 0 时把上游文案带出来。
fn unwrap_envelope(value: &Value, operation: &str) -> Result<Value, String> {
    if !value.is_object() {
        return Ok(value.clone());
    }
    if let Some(code) = value.get("code") {
        let zero =
            code.as_i64() == Some(0) || code.as_str() == Some("0") || code.as_u64() == Some(0);
        let missing = code.is_null();
        if !zero && !missing {
            let message = message_of(value);
            return Err(if message.is_empty() {
                format!("{operation}失败（上游返回 code {code}）")
            } else {
                format!("{operation}失败：{message}")
            });
        }
    }
    Ok(value.get("data").cloned().unwrap_or_else(|| value.clone()))
}

async fn get_value(
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
    path: &str,
    cookie: Option<&str>,
    operation: &str,
) -> Result<Value, String> {
    let url = format!("{}{}", AUTH_BASE, path);
    let mut request = client(proxy, gateway_proxy)
        .get(&url)
        .header("Accept", "application/json")
        .timeout(std::time::Duration::from_secs(20));
    if let Some(cookie) = cookie {
        request = request.header("Cookie", cookie);
    }
    let response = decorate(request)
        .send()
        .await
        .map_err(|error| format!("{operation}网络失败：{}", http::describe(&error)))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let value: Value = serde_json::from_str(&text)
        .unwrap_or_else(|_| json!({ "raw": text.chars().take(500).collect::<String>() }));
    if !(200..300).contains(&status) {
        let message = message_of(&value);
        return Err(if message.is_empty() {
            format!("{operation}失败（HTTP {status}）")
        } else {
            format!("{operation}失败：{message}")
        });
    }
    unwrap_envelope(&value, operation)
}

async fn post_form(
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
    path: &str,
    form: Form,
    cookie: Option<&str>,
    operation: &str,
) -> Result<Value, String> {
    let url = format!("{}{}", AUTH_BASE, path);
    let mut request = client(proxy, gateway_proxy)
        .post(&url)
        .header("Accept", "application/json")
        .timeout(std::time::Duration::from_secs(30))
        .multipart(form);
    if let Some(cookie) = cookie {
        request = request.header("Cookie", cookie);
    }
    let response = decorate(request)
        .send()
        .await
        .map_err(|error| format!("{operation}网络失败：{}", http::describe(&error)))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let value: Value = serde_json::from_str(&text)
        .unwrap_or_else(|_| json!({ "raw": text.chars().take(500).collect::<String>() }));
    if !(200..300).contains(&status) {
        let message = message_of(&value);
        return Err(if message.is_empty() {
            format!("{operation}失败（HTTP {status}）")
        } else {
            format!("{operation}失败：{message}")
        });
    }
    unwrap_envelope(&value, operation)
}

async fn post_empty(
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
    path: &str,
    cookie: &str,
    operation: &str,
) -> Result<Value, String> {
    let url = format!("{}{}", AUTH_BASE, path);
    let request = client(proxy, gateway_proxy)
        .post(&url)
        .header("Accept", "application/json")
        .header("Cookie", cookie)
        .timeout(std::time::Duration::from_secs(20));
    let response = decorate(request)
        .send()
        .await
        .map_err(|error| format!("{operation}网络失败：{}", http::describe(&error)))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let value: Value = serde_json::from_str(&text)
        .unwrap_or_else(|_| json!({ "raw": text.chars().take(500).collect::<String>() }));
    if !(200..300).contains(&status) {
        let message = message_of(&value);
        return Err(if message.is_empty() {
            format!("{operation}失败（HTTP {status}）")
        } else {
            format!("{operation}失败：{message}")
        });
    }
    unwrap_envelope(&value, operation)
}

fn captcha_form(captcha: &Captcha) -> Form {
    let seccode = base64_encode(captcha.seccode.as_bytes());
    Form::new()
        .text("geetest_challenge", captcha.challenge.clone())
        .text("geetest_validate", captcha.validate.clone())
        .text("geetest_seccode", seccode)
}

/// 极验配置（页面初始化用）。
pub async fn geetest_config(
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
) -> Result<Value, String> {
    get_value(
        proxy,
        gateway_proxy,
        "chat/gee-captcha",
        None,
        "读取极验配置",
    )
    .await
}

/// 发短信验证码（要极验）。
pub async fn send_sms(
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
    mobile: &str,
    captcha: &Captcha,
) -> Result<(), String> {
    let form = captcha_form(captcha)
        .text("mobile", mobile.to_string())
        .text("countryCode", "86");
    post_form(
        proxy,
        gateway_proxy,
        "login/mobile/send-verify-code",
        form,
        None,
        "发送验证码",
    )
    .await
    .map(|_| ())
}

/// 手机号 + 验证码登录（不需要极验）。
pub async fn login_sms(
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
    mobile: &str,
    verify_code: &str,
) -> Result<Session, String> {
    let form = Form::new()
        .text("mobile", mobile.to_string())
        .text("verifyCode", verify_code.to_string())
        .text("countryCode", "86");
    let value = post_form(
        proxy,
        gateway_proxy,
        "login/phone-quick-login",
        form,
        None,
        "手机号登录",
    )
    .await?;
    complete(proxy, gateway_proxy, &value).await
}

/// 账号 + 密码登录（要极验）。
pub async fn login_password(
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
    account: &str,
    password: &str,
    captcha: &Captcha,
) -> Result<Session, String> {
    let form = captcha_form(captcha)
        .text("accountName", account.to_string())
        .text("accountPwd", password.to_string());
    let value = post_form(
        proxy,
        gateway_proxy,
        "login/check-account",
        form,
        None,
        "账号密码登录",
    )
    .await?;
    complete(proxy, gateway_proxy, &value).await
}

/// 登录之后的收尾：租户 App、用户信息、模型凭据。
async fn complete(
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
    login: &Value,
) -> Result<Session, String> {
    let account_id = login
        .get("account_id")
        .or_else(|| login.get("accountId"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let sso_session_id = login
        .get("ssoSessionId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let token = login
        .get("token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| sso_session_id.clone());
    if account_id.is_empty() || token.is_empty() {
        return Err("AStudio 登录返回不完整（缺少账号或令牌）".to_string());
    }
    let mut session = Session {
        account_id,
        uid: login
            .get("uid")
            .map(|value| value.to_string().trim_matches('"').to_string())
            .unwrap_or_default(),
        token,
        sso_session_id,
        mobile: login
            .get("mobile")
            .or_else(|| login.get("phoneNum"))
            .or_else(|| login.get("phone"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        ..Default::default()
    };
    let cookie = session.cookie_header();
    // 租户 App：顺便拿封禁标记（桌面端是一次不带 body 的 POST）
    if let Ok(value) = post_empty(
        proxy,
        gateway_proxy,
        "tenant-app/v2/init-app",
        &cookie,
        "初始化账号",
    )
    .await
    {
        session.banned = value.get("banned").and_then(Value::as_bool) == Some(true);
    }
    if session.banned {
        return Err("AStudio 账号已被封禁".to_string());
    }
    // 用户信息：补 uid / mobile
    if session.uid.is_empty() || session.mobile.is_empty() {
        if let Ok(value) = get_value(
            proxy,
            gateway_proxy,
            "userInfo",
            Some(&cookie),
            "读取用户信息",
        )
        .await
        {
            let info = value.get("userInfo").unwrap_or(&value);
            if session.uid.is_empty() {
                if let Some(uid) = info.get("uid") {
                    session.uid = uid.to_string().trim_matches('"').to_string();
                }
            }
            if session.mobile.is_empty() {
                if let Some(mobile) = info
                    .get("mobile")
                    .or_else(|| info.get("telephone"))
                    .and_then(Value::as_str)
                {
                    session.mobile = mobile.to_string();
                }
            }
            if session.nickname.is_empty() {
                if let Some(name) = info
                    .get("nickname")
                    .or_else(|| info.get("username"))
                    .and_then(Value::as_str)
                {
                    session.nickname = name.to_string();
                }
            }
        }
    }
    // 模型凭据
    let value = get_value(
        proxy,
        gateway_proxy,
        "bot/models/configs",
        Some(&cookie),
        "读取模型凭据",
    )
    .await?;
    let list = value.as_array().cloned().unwrap_or_default();
    let pick = |predicate: fn(&Value) -> bool| -> Option<String> {
        list.iter()
            .find(|item| predicate(item))
            .and_then(|item| item.get("api_key").and_then(Value::as_str))
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
    };
    let key = pick(|item| item.get("is_current").and_then(Value::as_bool) == Some(true))
        .or_else(|| pick(|item| item.get("is_default").and_then(Value::as_bool) == Some(true)))
        .or_else(|| {
            list.iter()
                .find_map(|item| item.get("api_key").and_then(Value::as_str))
                .map(|key| key.trim().to_string())
                .filter(|key| !key.is_empty())
        });
    let Some(key) = key else {
        return Err("这个 AStudio 账号没有可用的模型凭据".to_string());
    };
    session.model_bearer_token = key;
    Ok(session)
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    STANDARD.encode(bytes)
}

/// 手机号形态（与桌面端的 `isValidAstronMobile` 同口径：中国大陆 11 位）。
pub fn valid_mobile(value: &str) -> bool {
    let bytes = value.trim().as_bytes();
    bytes.len() == 11 && bytes[0] == b'1' && bytes.iter().all(|byte| byte.is_ascii_digit())
}

/// 验证码形态（6 位数字）。
pub fn valid_code(value: &str) -> bool {
    let text = value.trim();
    text.len() == 6 && text.chars().all(|ch| ch.is_ascii_digit())
}
