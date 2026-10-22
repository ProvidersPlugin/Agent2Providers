//! LobsterAI 登录与续期：**浏览器授权 + 本机 loopback 回调 + 换码**。
//!
//! ── 四步 ────────────────────────────────────────────────────
//! ```text
//!   1. 本网关按 state 登记一轮登录，产出授权地址：
//!        {portal}/portal#/login?source=electron
//!                    &redirect_uri=http://127.0.0.1:<本网关端口>/auth/callback
//!                    &state=<state>
//!   2. 用户在浏览器里授权（手机号 / 微信），门户 302 回本机 loopback：
//!        GET /auth/callback?code=<code>&state=<state>
//!   3. POST {api}/api/auth/exchange   **不带 Authorization**
//!        体 { authCode, firstKeyfrom, latestKeyfrom, uuid, version }
//!   4. 落账号（把 uuid 与两个 keyfrom 一起存 —— 见 `credentials.rs` 的模块头）
//! ```
//!
//! ── 回调路径为什么不用 `-lobsterai` 后缀 ──────────────────────
//! 本仓其余几家自作主张的回调路径都带家名后缀（`/auth/callback-accio` 等），
//! 一眼能看出归属。这一家**例外**：授权地址里的 `redirect_uri` 是我们拼给门户的，
//! 门户会按它回落 —— 参考实现用的就是裸 `/auth/callback`（`lobsterai.ts:47`），
//! 而门户是否对回调路径有白名单**没有实测支撑**。所以这里跟随参考实现那条
//! 已验证的形状，不为了命名风格去赌一次未经验证的兼容性。该路径目前在本仓未被占用。
//!
//! ── 续期：**必须原样回传载荷件，且 `latestKeyfrom` 永不更新** ─────
//! `POST /api/auth/refresh` 同样**不带 Authorization**，体是
//! `{ firstKeyfrom, latestKeyfrom, version, [uuid], [userId], refreshToken }`。
//! 响应带新 `refreshToken` 时才轮换；`latestKeyfrom` **不覆盖**（照实测的形态，
//! 见 `credentials.rs` 的模块头）。
//!
//! ── 两种「成功状态下的失败」 ──────────────────────────────────
//!   1. 业务信封 `code != 0` → 按错误处理；
//!   2. 信封 `code == 0` 但 `data` 为 null → **令牌已失效**（参考实现实测）。
//!      这一条最容易漏：只看 HTTP 状态或只看 `code` 都会把它当成成功。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{Value, json};

use crate::http::send_raw;

use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;
use crate::logging;

use super::credentials::LobsterCredentials;
use super::endpoints::{
    CALLBACK_PATH, EXCHANGE_PATH, REQUEST_TIMEOUT_MS, REFRESH_PATH, USER_AGENT, api_base,
    portal_base, urlencode,
};

/// 本网关的 loopback 回调路径（见模块头：跟随参考实现的裸路径）
pub const LOOPBACK_CALLBACK_PATH: &str = CALLBACK_PATH;

/// pending 表有效期：与本仓登录任务的 5 分钟超时同一量级
const PENDING_TTL_MS: i64 = 6 * 60 * 1000;

/// 本机回调基址（端口未知时 None，错误由上层文案说清）。
///
/// ── 端口为什么从 `ctx.env` 取而不是进程级常量 ─────────────────
/// 网关在握手时把监听端口交给插件（`HostEnv.loopback_port`）。它进程级不变，
/// 所以每次从 `PluginCtx` 读一次是零成本的 —— 而进程级常量在插件里做不到
/// 「网关换了端口」时的正确性（插件进程可能跨网关重启存活）。
pub fn loopback_base(ctx: &PluginCtx) -> Option<String> {
    let port = ctx.env.loopback_port;
    if port == 0 {
        return None;
    }
    Some(format!("http://127.0.0.1:{port}"))
}

/// 本机回调完整地址（换码时要原样回传的 `redirect_uri`）
pub fn redirect_uri(ctx: &PluginCtx) -> Option<String> {
    loopback_base(ctx).map(|base| format!("{base}{LOOPBACK_CALLBACK_PATH}"))
}

/// 一轮待完成的登录（回调进来时按 state 取回）
pub struct PendingLogin {
    created_at: i64,
}

fn pending_table() -> &'static Mutex<HashMap<String, PendingLogin>> {
    static TABLE: OnceLock<Mutex<HashMap<String, PendingLogin>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_pending() -> std::sync::MutexGuard<'static, HashMap<String, PendingLogin>> {
    match pending_table().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 清掉过期的 pending（顺手做，不需要独立定时器）
fn sweep(table: &mut HashMap<String, PendingLogin>) {
    let now = logging::now_ms();
    table.retain(|_, entry| now - entry.created_at < PENDING_TTL_MS);
}

/// 授权地址：`{portal}/portal#/login?source=electron&redirect_uri=…&state=…`
pub fn build_authorize_url(return_url: &str, state: &str) -> String {
    format!(
        "{}/portal#/login?source=electron&redirect_uri={}&state={}",
        portal_base(),
        urlencode(return_url),
        urlencode(state)
    )
}

/// 登记一轮登录，返回 `(auth_url, state)`。
///
/// 同一时刻只保证「这一轮」可被回调命中：旧轮次留在表里到期自然清掉（用户可能
/// 开过好几个标签页），但**同一 state 只能被兑一次**（见 [`take_pending`]）。
pub fn begin_login(return_url: &str) -> (String, String) {
    let state = crate::ids::uuid_v4().replace('-', "");
    let auth_url = build_authorize_url(return_url, &state);
    let mut table = lock_pending();
    sweep(&mut table);
    table.insert(
        state.clone(),
        PendingLogin {
            created_at: logging::now_ms(),
        },
    );
    (auth_url, state)
}

/// 取走一轮登录（回调进来时调用；取走即失效，重复回调拿不到第二次）
pub fn take_pending(state: &str) -> Option<PendingLogin> {
    let mut table = lock_pending();
    sweep(&mut table);
    table.remove(state)
}

/// 丢弃一轮登录（取消 / 失败收尾）。
///
/// 插件的 `login.cancel` 是可选实现：本家的 pending 表 6 分钟自然过期，
/// 不显式丢弃也不会泄漏（表按 state 单条，量级等于并发登录数）。
#[allow(dead_code)]
pub fn drop_pending(state: &str) {
    let mut table = lock_pending();
    table.remove(state);
}

/// 从回调的 query 里取 `(code, state)`。
///
/// 门户可能带回额外参数（微信授权那条链会挂 `scope` 之类），所以按键找而不是按位置切。
pub fn parse_callback(query: &str) -> Option<(String, String)> {
    let mut code = String::new();
    let mut state = String::new();
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        let decoded = urldecode(value);
        match key {
            "code" => code = decoded,
            "state" => state = decoded,
            _ => {}
        }
    }
    if code.is_empty() || state.is_empty() {
        return None;
    }
    Some((code, state))
}

/// 极小的 query 解码（`+` 当空格，`%XX` 还原）。
///
/// 不引依赖：回调只有两三个参数，且值都是可见 ASCII 记号。
fn urldecode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 一次令牌包的解析结果（换码与续期共用）
#[derive(Clone, Debug, Default)]
pub struct TokenBundle {
    /// `accessToken`
    pub access_token: String,
    /// `refreshToken`（可能为空：续期响应不带新 token 时保留旧的）
    pub refresh_token: String,
    /// `expiresIn`（秒）
    pub expires_in: i64,
    /// 上游用户 id（`user.id`）—— 账号 id 与 keyfrom 都要它
    pub uid: String,
    /// `user.userId`
    pub user_id: String,
    /// `user.nickname`
    pub nickname: String,
}

/// 解业务信封 `{code, msg, data}`。
///
/// `code == 0` 且 `data` **非 null** 才算成功 —— `data: null` 配 `code: 0`
/// 是「令牌子失效」的形态（参考实现实测），把它当成功会让上层拿空令牌去发请求。
pub fn unwrap_envelope(payload: &Value, what: &str) -> Result<Value, GatewayError> {
    let code = payload.get("code").and_then(Value::as_i64).unwrap_or(-1);
    let message = payload
        .get("msg")
        .or_else(|| payload.get("message"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if code != 0 {
        let detail = if message.is_empty() {
            format!("code={code}")
        } else {
            message.to_string()
        };
        return Err(GatewayError::with_status(
            502,
            format!("LobsterAI {what}失败：{detail}"),
        ));
    }
    match payload.get("data") {
        Some(Value::Null) | None => Err(GatewayError::with_status(
            401,
            format!("LobsterAI {what}未返回数据（登录态可能已失效），请重新登录"),
        )),
        Some(data) => Ok(data.clone()),
    }
}

/// 从令牌信封的 `data` 里解出 [`TokenBundle`]。
pub fn parse_token_bundle(data: &Value) -> Result<TokenBundle, GatewayError> {
    let access_token = data
        .get("accessToken")
        .or_else(|| data.get("access_token"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if access_token.is_empty() {
        return Err(GatewayError::with_status(
            502,
            "LobsterAI 换码响应缺少 accessToken",
        ));
    }
    let user = data.get("user").cloned().unwrap_or(Value::Null);
    let str_of = |value: &Value, key: &str| -> String {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    Ok(TokenBundle {
        access_token,
        refresh_token: {
            let token = str_of(data, "refreshToken");
            if token.is_empty() {
                str_of(data, "refresh_token")
            } else {
                token
            }
        },
        expires_in: data
            .get("expiresIn")
            .and_then(Value::as_i64)
            .filter(|value| *value > 0)
            .unwrap_or(0),
        uid: {
            let id = str_of(&user, "id");
            if id.is_empty() {
                str_of(&user, "yid")
            } else {
                id
            }
        },
        user_id: str_of(&user, "userId"),
        nickname: str_of(&user, "nickname"),
    })
}

/// 用授权码换令牌（**不带 Authorization**）。
///
/// 体固定五个字段：`authCode` + 两个 keyfrom + `uuid` + `version`。
/// `uuid` 由调用方生成并**落盘**（续期要它）—— 它不是凭据，是这一台「客户端」的标识。
pub async fn exchange(
    auth_code: &str,
    uuid: &str,
    first_keyfrom: &str,
    latest_keyfrom: &str,
    client_version: &str,
) -> Result<TokenBundle, GatewayError> {
    let body = json!({
        "authCode": auth_code,
        "firstKeyfrom": first_keyfrom,
        "latestKeyfrom": latest_keyfrom,
        "uuid": uuid,
        "version": client_version,
    });
    let payload = post_anonymous(EXCHANGE_PATH, &body, "换码").await?;
    let data = unwrap_envelope(&payload, "换码")?;
    parse_token_bundle(&data)
}

/// 续期（**不带 Authorization**）：载荷件原样回传 + `refreshToken`。
pub async fn refresh(
    credentials: &LobsterCredentials,
    client_version: &str,
) -> Result<TokenBundle, GatewayError> {
    if !credentials.can_refresh() {
        return Err(GatewayError::with_status(
            401,
            "LobsterAI 账号缺少续期所需的载荷件（uuid / keyfrom 之一为空），请重新登录",
        ));
    }
    let mut body = serde_json::Map::new();
    for (key, value) in credentials.keyfrom_fields(client_version) {
        body.insert(key.to_string(), Value::String(value));
    }
    body.insert(
        "refreshToken".to_string(),
        Value::String(credentials.refresh_token.clone()),
    );
    let payload = post_anonymous(REFRESH_PATH, &Value::Object(body), "续期").await?;
    let data = unwrap_envelope(&payload, "续期")?;
    parse_token_bundle(&data)
}

/// 发一次匿名 POST（换码与续期都不带 Authorization）
async fn post_anonymous(path: &str, body: &Value, what: &str) -> Result<Value, GatewayError> {
    let url = format!("{}{}", api_base(), path);
    let response = send_raw(
        "POST",
        &url,
        Some(body),
        &[
            ("accept".to_string(), "application/json".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
            ("user-agent".to_string(), USER_AGENT.to_string()),
        ],
        None,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await
    .map_err(|error| {
        if error.is_timeout() {
            GatewayError::with_status(504, format!("LobsterAI {what}超时"))
        } else {
            GatewayError::with_status(502, format!("LobsterAI {what}请求失败: {error}"))
        }
    })?;
    let payload = response.payload.ok_or_else(|| {
        GatewayError::with_status(502, format!("LobsterAI {what}响应不是 JSON"))
    })?;
    // 401/403 直接归成登录态问题，别让信封里的文案把方向带偏
    if response.status == 401 || response.status == 403 {
        return Err(GatewayError::with_status(
            401,
            format!("LobsterAI {what}被拒（HTTP {}），请重新登录", response.status),
        ));
    }
    if !response.ok {
        return Err(GatewayError::with_status(
            response.status as i32,
            format!("LobsterAI {what}返回 HTTP {}", response.status),
        ));
    }
    Ok(payload)
}
