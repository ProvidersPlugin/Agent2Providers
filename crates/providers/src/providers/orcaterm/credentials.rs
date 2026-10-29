//! OrcaTerm 凭证：解析 / 本机登录态读取 / 续期 / 账号快照。
//!
//! ── 凭证形态（2026-10-16 桌面客户端 1.6.4 实测）────────────────
//! 微信 OAuth 登录后客户端持有一枚 HS256 JWT（`oauth_access_token`，约
//! **2 小时**时效），payload 里有 `userId`（数字 uid，CGI 的 `uin` 参数）、
//! `provider`、`iat` / `exp`。**只解 payload、不验签** —— token 好不好用
//! 由上游说了算。
//!
//! ── 本机登录态（`importDesktop: true`）──────────────────────
//! 客户端是 Tauri 应用，登录态在
//! `%APPDATA%\com.orcaterm-desktop.app\data.bin`（Tauri store，**明文 JSON**），
//! 直接读 `oauth_access_token` / `oauth_expires_at` / `auth_identity` 即可。
//!
//! ── 续期（上游有真续期接口）─────────────────────────────────
//! `OAuthRefreshToken` 传旧 token 换新（实测通过），因此
//! `refresh = true`：临期主动刷 + 401 强制刷，结果回写账号记录
//! （**不回写 data.bin**：那是桌面客户端自己的数据，它在线时随时可能覆盖）。

use std::path::PathBuf;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{PluginCtx, StoreAccount, StoreError};
use serde_json::{json, Value};

use super::http::{self, HttpFailure};
use super::{
    CGI_BASE, DESKTOP_DATA_FILE, ID, SECRET_EXPIRES_AT, SECRET_NAME, SECRET_TOKEN, SECRET_UID,
};
use crate::flight;

/// 临期窗口（10 分钟；JWT 实测 2 小时时效，留足一次对话的余量）。
const EXPIRY_WINDOW_MS: i64 = 10 * 60_000;

/// 登录态文件的大小上限（data.bin 正常约 1.5 KB；超限视为异常文件）。
const MAX_DATA_FILE_SIZE: u64 = 256 * 1024;

/// 一条 OrcaTerm 登录态。
#[derive(Clone, Debug)]
pub struct Credentials {
    /// OAuth access token（HS256 JWT，Bearer 头本体）。
    pub access_token: String,
    /// JWT `exp`（秒；解析不出给 0 —— 按「判不出」处理）。
    pub expires_at: i64,
    /// 数字用户 id（JWT `userId` / CGI 的 `uin` 参数）。
    pub user_id: String,
    /// 展示名（`auth_identity.nickName` / `GetUserInfo`，best-effort）。
    pub nickname: String,
}

impl Credentials {
    /// JWT 是否已过期或临期（`exp` 判不出 → false）。
    pub fn is_expiring(&self) -> bool {
        if self.expires_at <= 0 {
            return false;
        }
        let now = crate::http::now_ms() / 1000;
        self.expires_at - now < EXPIRY_WINDOW_MS / 1000
    }

    /// 脱敏尾段（展示用）。
    pub fn token_tail(&self) -> String {
        let token = self.access_token.trim();
        if token.len() <= 8 {
            return String::new();
        }
        token.chars().skip(token.len() - 8).collect()
    }
}

/// 归一化一个粘贴到的 token：剥 `Bearer ` / `ot_session=` 前缀与空白。
pub fn normalize_token(raw: &str) -> String {
    let mut token = raw.trim().to_string();
    if let Some(rest) = token.strip_prefix("ot_session=") {
        token = rest.trim().to_string();
    }
    if let Some(rest) = token.strip_prefix("Bearer ").or_else(|| token.strip_prefix("bearer ")) {
        token = rest.trim().to_string();
    }
    token
}

/// 解码 JWT 的 payload 段（base64url → JSON；失败返回 None）。
fn decode_jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    if payload.is_empty() {
        return None;
    }
    let mut base64 = payload.replace('-', "+").replace('_', "/");
    while base64.len() % 4 != 0 {
        base64.push('=');
    }
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD.decode(base64).ok()?;
    let claims = serde_json::from_slice::<Value>(&bytes).ok()?;
    claims.is_object().then_some(claims)
}

/// 从 JWT 里解出（userId, exp 秒）。
fn claims_of(token: &str) -> (String, i64) {
    let Some(claims) = decode_jwt_claims(token) else {
        return (String::new(), 0);
    };
    let user_id = claims
        .get("userId")
        .map(|value| match value {
            Value::Number(number) => number.to_string(),
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    let exp = claims.get("exp").and_then(Value::as_i64).unwrap_or(0);
    (user_id, exp)
}

/// 从粘贴的 payload 解析凭证。
///
/// 支持三种形态：裸 token / data.bin 的 JSON 对象 / 嵌套 `{accessToken: "…"}`。
pub fn credentials_from_payload(payload: &Value) -> Result<Credentials, String> {
    let token = match payload {
        Value::String(text) => normalize_token(text),
        Value::Object(object) => {
            let direct = ["accessToken", "token", "oauth_access_token"]
                .iter()
                .find_map(|key| object.get(*key).and_then(Value::as_str))
                .map(normalize_token)
                .unwrap_or_default();
            if direct.is_empty() {
                return Err(
                    "缺少 accessToken（粘贴 OrcaTerm 的 JWT，或整个 data.bin 的内容）".to_string()
                );
            }
            direct
        }
        _ => String::new(),
    };
    if token.split('.').count() != 3 {
        return Err("这不是一个有效的 OrcaTerm accessToken（JWT 应为三段点分）".to_string());
    }
    let (user_id, expires_at) = claims_of(&token);
    Ok(Credentials { access_token: token, expires_at, user_id, nickname: String::new() })
}

// ─── 本机登录态 ─────────────────────────────────────────────

/// 桌面端 data.bin 的候选路径（`ORCATERM_DATA_FILE` 优先，其次 `%APPDATA%`）。
fn data_file_candidates() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(value) = std::env::var_os("ORCATERM_DATA_FILE") {
        paths.push(PathBuf::from(value));
    }
    if let Some(value) = std::env::var_os("APPDATA") {
        paths.push(PathBuf::from(value).join(DESKTOP_DATA_FILE));
    }
    paths.into_iter().filter(|path| path.is_file()).collect()
}

/// 读本机桌面客户端登录态。
pub fn read_desktop_credentials() -> Result<Credentials, String> {
    let path = data_file_candidates().into_iter().next().ok_or_else(|| {
        "没有找到本机 OrcaTerm 客户端的登录态（%APPDATA%\\com.orcaterm-desktop.app\\data.bin）；请先安装并登录 OrcaTerm 桌面客户端，或用「粘贴 accessToken」添加账号".to_string()
    })?;
    let meta = std::fs::symlink_metadata(&path)
        .map_err(|error| format!("读取客户端登录态失败（{}）：{error}", path.display()))?;
    if meta.len() > MAX_DATA_FILE_SIZE {
        return Err(format!(
            "客户端登录态文件异常过大（{}，{} 字节），拒绝读取",
            path.display(),
            meta.len()
        ));
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("读取客户端登录态失败（{}）：{error}", path.display()))?;
    let Ok(data) = serde_json::from_str::<Value>(&text) else {
        return Err(format!("客户端登录态文件不是有效 JSON（{}）", path.display()));
    };
    let token = normalize_token(
        data.get("oauth_access_token").and_then(Value::as_str).unwrap_or(""),
    );
    if token.is_empty() {
        return Err(
            "本机客户端未登录（data.bin 里没有 oauth_access_token）；请先在 OrcaTerm 客户端登录"
                .to_string(),
        );
    }
    let file_expires = data.get("oauth_expires_at").and_then(Value::as_i64)
        .map(expiry_seconds).unwrap_or(0);
    let (user_id, jwt_exp) = claims_of(&token);
    let expires_at = if jwt_exp > 0 { jwt_exp } else { file_expires };
    // `auth_identity` 是一段内嵌 JSON 字符串（`{"nickName":"…"}`）
    let nickname = data
        .get("auth_identity")
        .and_then(Value::as_str)
        .and_then(|identity| serde_json::from_str::<Value>(identity).ok())
        .and_then(|identity| identity.get("nickName").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    Ok(Credentials { access_token: token, expires_at, user_id, nickname })
}

// ─── 上游请求头 ─────────────────────────────────────────────

/// CGI 网关的请求头（`uin` 在 query 上，不在头里）。
pub fn cgi_headers(credentials: &Credentials) -> Vec<(String, String)> {
    vec![
        ("Authorization".to_string(), format!("Bearer {}", credentials.access_token)),
        ("Origin".to_string(), "https://orcaterm.cloud.tencent.com".to_string()),
        ("Referer".to_string(), "https://orcaterm.cloud.tencent.com/".to_string()),
    ]
}

/// assistant 网关的请求头（`X-Product: orcaterm` 是本家的产品标识）。
pub fn lightai_headers(credentials: &Credentials, seq_id: &str) -> Vec<(String, String)> {
    vec![
        ("Authorization".to_string(), format!("Bearer {}", credentials.access_token)),
        ("X-Product".to_string(), "orcaterm".to_string()),
        ("x-seqid".to_string(), seq_id.to_string()),
        ("Origin".to_string(), "https://orcaterm.cloud.tencent.com".to_string()),
        ("Referer".to_string(), "https://orcaterm.cloud.tencent.com/".to_string()),
    ]
}

// ─── CGI 动作 ───────────────────────────────────────────────

/// 调一次 CGI 动作（`POST {CGI}/cgi/api?action=<Action>&uin=<uid>`）。
///
/// HTTP 恒 200，业务码在 `data.Response.Error` —— 这里只拆出
/// `data.Response`，错误判定归调用方。
pub async fn cgi_action(
    client: &reqwest::Client,
    credentials: &Credentials,
    action: &str,
    data: Value,
) -> Result<Value, HttpFailure> {
    let url = format!("{CGI_BASE}/cgi/api?action={action}&uin={}", credentials.user_id);
    let body = json!({ "action": action, "data": data });
    let value = http::post_json(client, &url, &cgi_headers(credentials), &body).await?;
    Ok(value.get("data").and_then(|data| data.get("Response")).cloned().unwrap_or(Value::Null))
}

/// 调一次**匿名** CGI 动作（不带 `Authorization`，也不带 `uin`）。
///
/// `OAuthExchangeToken` 是登录**之前**的动作：此时还没有令牌可带，凭据就是
/// 请求体里的 `sessionId` 本身。
pub async fn cgi_action_anonymous(
    client: &reqwest::Client,
    action: &str,
    data: Value,
) -> Result<Value, HttpFailure> {
    let url = format!("{CGI_BASE}/cgi/api?action={action}");
    let headers = vec![
        ("Origin".to_string(), "https://orcaterm.cloud.tencent.com".to_string()),
        ("Referer".to_string(), "https://orcaterm.cloud.tencent.com/".to_string()),
    ];
    let body = json!({ "action": action, "data": data });
    let value = http::post_json(client, &url, &headers, &body).await?;
    Ok(value.get("data").and_then(|data| data.get("Response")).cloned().unwrap_or(Value::Null))
}

/// `GetUserInfo`（探活 + 补身份）。返回 `(userId, nickname)`。
pub async fn fetch_user_info(
    client: &reqwest::Client,
    credentials: &Credentials,
) -> Result<(String, String), HttpFailure> {
    let response = cgi_action(client, credentials, "GetUserInfo", json!({})).await?;
    if let Some(error) = response.get("Error") {
        let code = error.get("Code").and_then(Value::as_str).unwrap_or("UNKNOWN");
        let message = error.get("Message").and_then(Value::as_str).unwrap_or("上游错误");
        return Err(HttpFailure {
            status: 401,
            message: format!("OrcaTerm 凭证被上游拒绝（{code}：{message}）"),
        });
    }
    let user_id = response
        .get("userId")
        .map(|value| match value {
            Value::Number(number) => number.to_string(),
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .unwrap_or_else(|| credentials.user_id.clone());
    // 展示名按「第一个非空的候选」取：上游这三段的**存在性并不稳定** ——
    // 微信登录实测是 `nickName: ""` + `username: "一颗苹果"`，只按键存在与否
    // 判断会把昵称丢成空串。
    let nickname = ["nickName", "username", "displayName"]
        .iter()
        .filter_map(|key| response.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or("")
        .to_string();
    Ok((user_id, nickname))
}

// ─── 账号快照 ───────────────────────────────────────────────

fn expiry_seconds(raw: i64) -> i64 {
    if raw >= 100_000_000_000 { raw / 1000 } else { raw }
}

/// 从账号 `secret` 解析凭证。
pub fn credentials_of(secret: &Value) -> Result<Credentials, String> {
    let token = normalize_token(secret.get(SECRET_TOKEN).and_then(Value::as_str).unwrap_or(""));
    if token.is_empty() {
        return Err(
            "OrcaTerm 账号缺少 accessToken：请重新导入或重新粘贴该账号".to_string()
        );
    }
    let (jwt_user_id, jwt_exp) = claims_of(&token);
    Ok(Credentials {
        access_token: token,
        expires_at: secret
            .get(SECRET_EXPIRES_AT)
            .and_then(Value::as_i64)
            .filter(|value| *value > 0)
            .map(expiry_seconds)
            .unwrap_or(jwt_exp),
        user_id: secret
            .get(SECRET_UID)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or(jwt_user_id),
        nickname: secret.get(SECRET_NAME).and_then(Value::as_str).unwrap_or("").to_string(),
    })
}

/// 从网关取一条账号并解析凭证。
pub async fn snapshot(ctx: &PluginCtx, account: &str) -> Result<Credentials, String> {
    let record = ctx
        .store_get(ID, account)
        .await
        .map_err(|error| describe_store_error(&error))?;
    credentials_of(&record.secret)
}

/// 把 `StoreError` 翻成客户端可见的文案。
fn describe_store_error(error: &StoreError) -> String {
    match error {
        StoreError::NotFound => {
            "OrcaTerm 没有可用的登录凭证：请在账号页添加账号（粘贴 accessToken 或导入桌面端登录态）"
                .to_string()
        }
        other => format!("读取 OrcaTerm 账号失败：{other}"),
    }
}

// ─── 续期（单飞 + 回写）─────────────────────────────────────

/// 刷新 JWT（单飞；成功后回写账号记录）。
///
/// `force = false` 时只在临期才刷新；`force = true` 时无条件刷新
/// （401 之后的那条路）。
pub async fn refresh(
    ctx: &PluginCtx,
    credentials: &Credentials,
    account: &str,
    force: bool,
) -> Result<Credentials, String> {
    if !force && !credentials.is_expiring() {
        return Ok(credentials.clone());
    }
    let account_id = ctx.store_get(ID, account).await
        .map_err(|error| describe_store_error(&error))?.id;
    let key = format!("{ID}:{account_id}");
    flight::run(&key, || async {
        let record = ctx.store_get(ID, &account_id).await
            .map_err(|error| describe_store_error(&error))?;
        let current = credentials_of(&record.secret)?;
        if !current.is_expiring()
            && (current.access_token != credentials.access_token || !force)
        {
            return Ok(current);
        }
        let next = call_refresh_api(ctx, &current).await?;
        write_back(ctx, &record, &next).await
    })
    .await
}

/// 调 `OAuthRefreshToken` 换新 token。
async fn call_refresh_api(ctx: &PluginCtx, credentials: &Credentials) -> Result<Credentials, String> {
    let client = crate::http::control_client(None, &ctx.env.proxy);
    let response = cgi_action(
        &client,
        credentials,
        "OAuthRefreshToken",
        json!({ "accessToken": credentials.access_token }),
    )
    .await
    .map_err(|error| error.message)?;
    let code = response.get("Code").and_then(Value::as_i64).unwrap_or(-1);
    if code != 0 {
        let message = response.get("Message").and_then(Value::as_str).unwrap_or("未知错误");
        return Err(format!(
            "OrcaTerm 续期失败（code={code}：{message}）；请重新导入桌面端登录态或重新粘贴 accessToken"
        ));
    }
    let token = normalize_token(response.get("AccessToken").and_then(Value::as_str).unwrap_or(""));
    if token.is_empty() {
        return Err("OrcaTerm 续期响应缺少 AccessToken".to_string());
    }
    let (user_id, expires_at) = claims_of(&token);
    let expires_at = if expires_at > 0 {
        expires_at
    } else {
        response.get("ExpiresIn").and_then(Value::as_i64)
            .filter(|value| *value > 0)
            .map(|seconds| (crate::http::now_ms() / 1000).saturating_add(seconds))
            .unwrap_or(0)
    };
    Ok(Credentials {
        access_token: token,
        expires_at,
        user_id: if user_id.is_empty() { credentials.user_id.clone() } else { user_id },
        nickname: credentials.nickname.clone(),
    })
}

/// 回写用的 `secret` 形状（`tokenExpireAt` 统一为 Unix 秒）。
pub fn secret_of(credentials: &Credentials) -> Value {
    json!({
        SECRET_TOKEN: credentials.access_token,
        SECRET_EXPIRES_AT: credentials.expires_at,
        SECRET_UID: credentials.user_id,
        SECRET_NAME: credentials.nickname,
    })
}

/// 条件回写账号记录，成功只返回已落盘的凭证。
async fn write_back(
    ctx: &PluginCtx,
    record: &StoreAccount,
    credentials: &Credentials,
) -> Result<Credentials, String> {
    let next = crate::store::with_fields(&record.secret, &[
        (SECRET_TOKEN, Value::String(credentials.access_token.clone())),
        (SECRET_EXPIRES_AT, Value::from(credentials.expires_at)),
        (SECRET_UID, Value::String(credentials.user_id.clone())),
        (SECRET_NAME, Value::String(credentials.nickname.clone())),
    ]);
    match ctx.store_update(&record.id, &record.secret, &next).await {
        Ok(true) => Ok(credentials.clone()),
        Ok(false) => {
            let latest = ctx.store_get(ID, &record.id).await.map_err(|error| {
                let message = format!("OrcaTerm 续期回写冲突后读取账号 {} 失败：{error}", record.id);
                agent2api_provider_sdk::log_warn!("{message}");
                message
            })?;
            let current = credentials_of(&latest.secret)?;
            if current.access_token == credentials.access_token
                && current.expires_at == credentials.expires_at
                || !current.is_expiring()
                    && current.access_token != normalize_token(record.secret.get(SECRET_TOKEN)
                        .and_then(Value::as_str).unwrap_or(""))
            {
                return Ok(current);
            }
            let message = format!("OrcaTerm 账号 {} 的续期结果未能落盘，请稍后重试", record.id);
            agent2api_provider_sdk::log_warn!("{message}");
            Err(message)
        }
        Err(error) => {
            let message = format!("OrcaTerm 账号 {} 的续期结果写回失败：{error}", record.id);
            agent2api_provider_sdk::log_warn!("{message}");
            Err(message)
        }
    }
}

// ─── 账号规范化 / 视图 ──────────────────────────────────────

/// `account.normalize`：把粘贴 / 导入的凭据变成一条可落库的账号。
pub async fn normalize(
    ctx: &PluginCtx,
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, String> {
    let (mut credentials, desktop) = match &input.source {
        proto::NormalizeSource::Desktop => (read_desktop_credentials()?, true),
        proto::NormalizeSource::Manual { payload } => {
            // 表单里的令牌可能在 `accessToken` / `token` / 裸字符串上
            let value = payload
                .get("accessToken")
                .or_else(|| payload.get("token"))
                .cloned()
                .unwrap_or_else(|| payload.clone());
            (credentials_from_payload(&value)?, false)
        }
        proto::NormalizeSource::FileText { content, .. } => {
            // 粘贴的可能是裸 token、也可能是一整份 data.bin 的 JSON
            let payload = serde_json::from_str::<Value>(content)
                .unwrap_or_else(|_| Value::String(content.clone()));
            (credentials_from_payload(&payload)?, false)
        }
        proto::NormalizeSource::FileBase64 { content, .. } => {
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(content)
                .map_err(|error| format!("文件内容不是合法 base64：{error}"))?;
            let text = String::from_utf8(bytes)
                .map_err(|_| "data.bin 不是 UTF-8 文本".to_string())?;
            let payload = serde_json::from_str::<Value>(&text)
                .map_err(|error| format!("data.bin 不是合法 JSON：{error}"))?;
            (credentials_from_payload(&payload)?, false)
        }
    };

    // ── 尽力补身份（不阻断添加）─────────────────────────────
    // 探活失败仍允许落库：token 可能刚过期而用户马上就要续期，或者上游
    // 临时抖动。真正不可用的凭证会在转发时给出准确文案。
    let client = crate::http::control_client(input.proxy.as_ref(), &ctx.env.proxy);
    if let Ok((user_id, nickname)) = fetch_user_info(&client, &credentials).await {
        if !user_id.is_empty() {
            credentials.user_id = user_id;
        }
        if !nickname.is_empty() {
            credentials.nickname = nickname;
        }
    }

    let name = input
        .name
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            if credentials.nickname.trim().is_empty() {
                format!("OrcaTerm {}", credentials.token_tail())
            } else {
                credentials.nickname.clone()
            }
        });
    let identity = identity_of(&secret_of(&credentials));
    Ok(proto::NormalizedAccount {
        secret: secret_of(&credentials),
        public: super::public_view(&name, &credentials.user_id, &credentials.token_tail()),
        identity,
        desktop,
    })
}

/// 账号的稳定身份（去重用）：`orcaterm-<userId>`，判不出用 token 尾。
pub fn identity_of(secret: &Value) -> String {
    let uid = secret.get(SECRET_UID).and_then(Value::as_str).unwrap_or("").trim();
    if !uid.is_empty() {
        return format!("orcaterm-{uid}");
    }
    match credentials_of(secret) {
        Ok(credentials) => format!("orcaterm-{}", credentials.token_tail()),
        Err(_) => String::new(),
    }
}

/// 从 `secret` 重新生成公开视图。
pub fn account_view(secret: &Value) -> Result<Value, String> {
    let credentials = credentials_of(secret)?;
    let name = secret
        .get(SECRET_NAME)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("OrcaTerm {}", credentials.token_tail()));
    Ok(super::public_view(&name, &credentials.user_id, &credentials.token_tail()))
}
