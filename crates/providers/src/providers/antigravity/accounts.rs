//! Antigravity 账号面：归一化 + 公开视图 + 稳定身份。
//!
//! ── 两条添加路径（对应网关 `api::accounts` 的 antigravity 分支）────
//! ```text
//!   Manual{payload}  粘贴 refresh token → 一次真实校验刷新（`login::verify_paste`）
//!                    → best-effort 发现 project / 补 email → 落账号
//!   Desktop          不支持（Google 的登录态在浏览器 / 客户端自己的加密存储里）
//!   FileText/Base64  不支持（凭证是令牌，不是文件）
//! ```
//! 网页登录是另一条链（`login.rs` → `oauth::exchange_code` → 同一个
//! [`account_of`]）。
//!
//! ── 身份怎么认（email → 令牌摘要）────────────────────────────
//! 网关那份的论证逐字保留：email 是对同一个 Google 账号**稳定**的标识 ——
//! 重新授权会换一枚新的 refresh token，但 email 不变，按 email 派生 id 才能
//! 让「重新粘贴」更新同一条记录，而不是留下一堆孤儿账号；email 缺失（用户只
//! 粘了令牌）时退到令牌摘要：同一枚令牌再粘一次仍命中同一条记录。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。

use serde_json::{Map, Value};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;

use super::credentials::{self, AntigravityCredentials};

/// 账号名长度上限
const MAX_NAME_LENGTH: usize = 100;

/// 添加账号入口。
pub(super) async fn normalize(
    ctx: &PluginCtx,
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, String> {
    let payload = match input.source {
        proto::NormalizeSource::Manual { payload } => payload,
        proto::NormalizeSource::Desktop => {
            return Err(
                "Antigravity 不支持导入桌面端登录态，请用「网页登录」或粘贴 Google refresh token 添加"
                    .to_string(),
            )
        }
        proto::NormalizeSource::FileText { .. } | proto::NormalizeSource::FileBase64 { .. } => {
            return Err(
                "Antigravity 没有可导入的凭证文件（凭证是 Google refresh token），请直接粘贴"
                    .to_string(),
            )
        }
    };
    // 校验刷新是**必需**的：「用户粘了一串东西」与「这串能用」之间没有本地
    // 判据（不像 API Key 有前缀形态）。不校验就落账号，用户会在第一次转发时
    // 才发现串是坏的 —— 那时错误文案混在转发链路里，比在添加页说清楚难查。
    let credentials = super::login::verify_paste(&payload, input.proxy.as_ref()).await?;
    let _ = ctx;
    let name = input
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| crate::json::truncate_text(value, MAX_NAME_LENGTH));
    Ok(account_of(&credentials, "manual", name))
}

/// 凭证 → 可落库的账号（网页登录与手工粘贴两条路共用）。
pub(super) fn account_of(
    credentials: &AntigravityCredentials,
    source: &str,
    explicit_name: Option<String>,
) -> proto::NormalizedAccount {
    let mut secret = Map::new();
    secret.insert(
        "refreshToken".to_string(),
        Value::String(credentials.refresh_token.clone()),
    );
    if !credentials.access_token.trim().is_empty() {
        secret.insert(
            "accessToken".to_string(),
            Value::String(credentials.access_token.clone()),
        );
    }
    if credentials.expires_at > 0 {
        secret.insert("expiresAt".to_string(), Value::from(credentials.expires_at));
    }
    if !credentials.project_id.trim().is_empty() {
        secret.insert(
            "projectId".to_string(),
            Value::String(credentials.project_id.clone()),
        );
    }
    if !credentials.email.trim().is_empty() {
        secret.insert("email".to_string(), Value::String(credentials.email.clone()));
        // userId = 本家的稳定身份（网关「从其他工具导入 / 导出」那条链路按
        // 通用字段 `userId` 认身份；Google 没有 userId 字段，用 email 顶）
        secret.insert("userId".to_string(), Value::String(credentials.email.clone()));
    }
    secret.insert(
        "tokenTail".to_string(),
        Value::String(crate::store::token_tail(&credentials.refresh_token)),
    );
    secret.insert("source".to_string(), Value::String(source.to_string()));
    // 名字：显式给的 > 上游 email > 令牌尾号
    let name = explicit_name
        .or_else(|| Some(credentials.email.clone()).filter(|value| !value.is_empty()))
        .unwrap_or_else(|| format!("Antigravity {}", credentials.masked_refresh()));
    secret.insert(
        "name".to_string(),
        Value::String(crate::json::truncate_text(&name, MAX_NAME_LENGTH)),
    );
    let secret = Value::Object(secret);
    proto::NormalizedAccount {
        public: account_view(&secret),
        identity: identity_of(&secret),
        secret,
        // 本家没有「实时读本机客户端」这条来源，恒 false
        desktop: false,
    }
}

/// Antigravity 专属的公开字段；账号 id 与 provider 由网关注入。
pub(super) fn account_view(secret: &Value) -> Value {
    let refresh_token = crate::store::pick(secret, &["refreshToken", "refresh_token"]);
    let access_token = crate::store::pick(secret, &["accessToken", "access_token"]);
    let mut view = serde_json::json!({
        "name": crate::store::pick(secret, &["name"]),
        "userId": crate::store::pick(secret, &["userId", "email"]),
        "email": crate::store::pick(secret, &["email"]),
        "projectId": crate::store::pick(secret, &["projectId", "project_id"]),
        "source": crate::store::pick(secret, &["source"]),
        // 展示用的尾号取 **refresh token**（本家的主凭证；access 只活一小时，
        // 按它算尾号会让同一账号每次刷新后显示不同）
        "tokenTail": crate::store::token_tail(&refresh_token),
        "hasRefreshToken": !refresh_token.is_empty(),
        "refreshTokenLongLived": true,
        "hasAccessToken": !access_token.is_empty(),
        "desktop": false,
        // 有 refreshToken 就算「当前可用」（accessToken 可以由刷新补齐）
        "available": !refresh_token.is_empty(),
    });
    let expires_at = ["expiresAt", "expires_at"]
        .iter()
        .find_map(|key| secret.get(*key).and_then(Value::as_i64))
        .filter(|value| *value > 0);
    if let Some(object) = view.as_object_mut() {
        if let Some(expires_at) = expires_at {
            object.insert("expiresAt".to_string(), Value::from(expires_at));
        }
    }
    view
}

/// 账号的稳定身份（`antigravity-<email>` 或 `antigravity-<令牌摘要>`）。
///
/// 与网关 `account_id_for` 的取值链逐字同源（email 优先、令牌摘要兜底），
/// 只是把网关那份 `sha256("antigravity:account-id:v1\0" + identity)[..12]`
/// 换成了可读前缀 —— 插件侧的身份只用于去重，不落成文件名，可读性更有用。
pub(super) fn identity_of(secret: &Value) -> String {
    let email = crate::store::pick(secret, &["email", "userId", "user_id"]);
    if !email.is_empty() {
        return format!("antigravity-{email}");
    }
    let refresh_token = crate::store::pick(secret, &["refreshToken", "refresh_token"]);
    if refresh_token.is_empty() {
        return String::new();
    }
    format!(
        "antigravity-{}",
        crate::flight::fingerprint(&refresh_token)
    )
}

/// 从账号记录还原凭证（转发 / 余额 / 目录三条链共用）。
///
/// `#[allow(dead_code)]`：三条链现在都直接调 `credentials::from_record`
/// （各自那一处是唯一入口）。留着它是为了给「从原始记录直接取凭证」这类
/// 调用点一个具名入口，避免第四处再写一遍取值链。
#[allow(dead_code)]
pub(super) fn credentials_of(record: &Value) -> Result<AntigravityCredentials, GatewayError> {
    credentials::from_record(Some(record))
}
