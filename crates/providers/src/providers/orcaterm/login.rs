//! OrcaTerm 登录：官方 OAuth 链路（微信扫码 → 回调 → 换令牌）。
//!
//! ── 链路（2026-10-17 实测）────────────────────────────────
//!   1. 打开 `https://orcaterm.com/oauth/authorize?provider=wechat&source=desktop
//!      &session_id=<uuid>&return_url=orcaterm://oauth_callback` —— 该页
//!      **302 直达**微信二维码，不需要点按钮；
//!   2. 用户扫码；上游把浏览器重定向到 `return_url` 并带上 `session_id`；
//!   3. 网关把回调 URL 交给 `login.callback`，本模块用 `OAuthExchangeToken`
//!      换令牌（该 CGI 动作**匿名可调**，凭据就是那个一次性 `session_id`）。
//!
//! ── 与「回调带 code」的家不同 ───────────────────────────────
//! 这里的回调 URL 只带 `session_id`（我们自己生成的 UUID，既是 CSRF 凭据
//! 也是换令牌的入参）。所以 [`start`] 生成的 `flow` 与 URL 里的
//! `session_id` 是**同一个值** —— 回调侧只能看到 URL 里那个，两者分开就要
//! 在插件里再存一份映射表（而它们本来就一一对应）。

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::credentials::{self, Credentials};
use super::CGI_BASE;

/// 回调地址（自定义协议）。上游把登录成功后的浏览器重定向到它。
pub const CALLBACK_URL: &str = "orcaterm://oauth_callback";

/// 回调 URL 里承载一次性凭据的参数名。
pub const CALLBACK_QUERY_KEY: &str = "session_id";

/// 登录窗口需要放行的域（网关据此做导航白名单）。
pub const ALLOWED_HOSTS: &[&str] = &["orcaterm.com", "open.weixin.qq.com", "wx.qq.com"];

/// 生成登录 state / OAuth session_id（**必须是合法 UUID**）。
///
/// ── 为什么必须是 UUID ──────────────────────────────────────
/// 实测上游对 `sessionId` 做格式校验，非 UUID 直接回
/// `api-params-verify-fail: sessionId must be a UUID`。这里把随机字节切成
/// 8-4-4-4-12 的十六进制形态（不引入 uuid 依赖）。
pub fn new_state() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::getrandom(&mut bytes);
    // 版本位与变体位按 UUID v4 写（上游只校验形态，但既然叫 UUID 就写合规）
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// 授权页地址（`session_id` 进 query，`return_url` 做最小转义）。
pub fn build_url(state: &str) -> String {
    format!(
        "https://orcaterm.com/oauth/authorize?provider=wechat&source=desktop&session_id={state}&return_url={}",
        encode(CALLBACK_URL)
    )
}

/// 最小 URL 编码（`return_url` 的取值里只有 `:` 与 `/` 需要转义）。
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        match ch {
            ':' => out.push_str("%3A"),
            '/' => out.push_str("%2F"),
            '?' => out.push_str("%3F"),
            '&' => out.push_str("%26"),
            '=' => out.push_str("%3D"),
            other => out.push(other),
        }
    }
    out
}

/// 从回调 URL 里取出 `session_id`。
pub fn callback_session_id(callback_url: &str) -> Option<String> {
    let query = callback_url.split_once('?')?.1;
    for part in query.split('&') {
        let Some((key, value)) = part.split_once('=') else { continue };
        if key == CALLBACK_QUERY_KEY && !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// `login.start`：只生成授权地址（回调型流程，不轮询）。
pub fn start(method: &str, payload: &Value) -> Result<proto::LoginStart, String> {
    match method {
        // 「浏览器登录」是回调型：网关打开 URL，等用户完成后调 login.callback
        "browser" | "" => {
            let state = new_state();
            Ok(proto::LoginStart {
                flow: state.clone(),
                url: build_url(&state),
                instructions: "用微信扫码完成登录；扫码成功后会自动回到应用".to_string(),
                poll_interval_ms: 0,
                // 微信扫码页在内嵌窗口里可用（实测 302 直达二维码），
                // 但第三方登录偶尔会拒绝 WebView —— 交给系统浏览器更稳。
                external_browser: true,
                allowed_hosts: ALLOWED_HOSTS.iter().map(|host| host.to_string()).collect(),
            })
        }
        // 「桌面端导入」不需要任何交互：网关直接调 normalize(Desktop)
        "desktop" => Ok(proto::LoginStart {
            flow: String::new(),
            url: String::new(),
            instructions: "将读取本机 OrcaTerm 客户端的登录态".to_string(),
            poll_interval_ms: 0,
            external_browser: false,
            allowed_hosts: Vec::new(),
        }),
        other => {
            // 粘贴令牌也是 login.start 的一种：`payload.token` 里有值就直接校验
            let token = payload.get("token").and_then(Value::as_str).unwrap_or("");
            if token.trim().is_empty() {
                return Err(format!("OrcaTerm 不支持这种登录方式（{other}）"));
            }
            Err("OrcaTerm 的粘贴登录请用「添加账号」，不需要走登录流程".to_string())
        }
    }
}

/// `login.callback`：用 `session_id` 换令牌并产出账号。
pub async fn callback(ctx: &PluginCtx, callback: proto::LoginCallback) -> Result<proto::LoginPoll, String> {
    // 回调 URL 里可能带 session_id（壳侧把整条 URL 交回来），也可能只有 code
    let session_id = callback_session_id(&callback.url)
        .or_else(|| callback_session_id(&callback.code))
        .unwrap_or_else(|| callback.code.clone());
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return Ok(proto::LoginPoll::Failed {
            message: "回调里没有 session_id，无法换取登录态".to_string(),
        });
    }
    // 逐字比对发起时那个 flow（深度防御：上游已校验过 state，但「只有本模块
    // 知道该长什么样」的校验放在这里最可靠）。
    if !callback.flow.is_empty() && session_id != callback.flow {
        return Ok(proto::LoginPoll::Failed {
            message: "回调的 session_id 与本次登录不匹配，请重新发起网页登录".to_string(),
        });
    }
    match exchange_session(ctx, session_id).await {
        Ok(credentials) => {
            let secret = credentials::secret_of(&credentials);
            Ok(proto::LoginPoll::Done {
                account: proto::NormalizedAccount {
                    public: super::public_view(
                        &display_name(&credentials),
                        &credentials.user_id,
                        &credentials.token_tail(),
                    ),
                    identity: credentials::identity_of(&secret),
                    secret,
                    desktop: false,
                },
            })
        }
        Err(message) => Ok(proto::LoginPoll::Failed { message }),
    }
}

/// 账号展示名（昵称优先，否则按令牌尾）。
fn display_name(credentials: &Credentials) -> String {
    if credentials.nickname.trim().is_empty() {
        format!("OrcaTerm {}", credentials.token_tail())
    } else {
        credentials.nickname.clone()
    }
}

/// 用一次性 `session_id` 换令牌，并探活补齐身份。
///
/// ── 为什么敢匿名调 ──────────────────────────────────────────
/// 实测该动作匿名可调，凭据就是 `session_id` 本身（一次性、由本进程生成、
/// 只出现在我们自己窗口的 URL 里）—— 安全性由「不可预测 + 一次性」承担。
async fn exchange_session(ctx: &PluginCtx, session_id: &str) -> Result<Credentials, String> {
    let client = crate::http::control_client(None, &ctx.env.proxy);
    let response = credentials::cgi_action_anonymous(
        &client,
        "OAuthExchangeToken",
        json!({ "sessionId": session_id }),
    )
    .await
    .map_err(|error| format!("OrcaTerm 换取登录态失败：{}（{CGI_BASE}）", error.message))?;
    if let Some(error) = response.get("Error") {
        let code = error.get("Code").and_then(Value::as_str).unwrap_or("UNKNOWN");
        let message = error.get("Message").and_then(Value::as_str).unwrap_or("上游错误");
        // 会话过期是用户可行动的错误（重新扫一次），给一句更直白的文案
        if code == "TOKEN_EXPIRED" {
            return Err(
                "OrcaTerm 登录会话已过期（扫码可能没完成或等待过久），请重新发起网页登录"
                    .to_string(),
            );
        }
        return Err(format!("OrcaTerm 换取登录态失败（{code}：{message}）"));
    }
    let token = response
        .get("AccessToken")
        .or_else(|| response.get("accessToken"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if token.split('.').count() != 3 {
        return Err("OrcaTerm 换回来的登录态不是有效令牌（JWT 应为三段点分），请重试".to_string());
    }
    let mut parsed = credentials::credentials_from_payload(&Value::String(token.clone()))?;
    parsed.access_token = token;
    // 探活补齐身份：落账号要用 nickname / user_id，且探活能兜住「铸造成功
    // 但账号被冻结」这类边缘情形（与粘贴入口同一口径）。
    let (user_id, nickname) = credentials::fetch_user_info(&client, &parsed)
        .await
        .map_err(|error| error.message)?;
    if parsed.user_id.is_empty() {
        parsed.user_id = user_id;
    }
    if parsed.nickname.is_empty() {
        parsed.nickname = nickname;
    }
    Ok(parsed)
}
