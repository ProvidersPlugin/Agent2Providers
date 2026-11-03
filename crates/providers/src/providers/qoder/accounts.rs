//! 账号面：设备授权收尾 / PAT 兑换 + 归一化 + 公开视图。
//!
//! ── 两条添加路径（对应网关 `api::accounts` 的分派）──────────────
//! ```text
//!   Manual{payload}  粘贴凭证（Qoder 还可以粘贴 PAT）→ 拉资料 → 落账号
//!   Desktop          不支持（没有客户端登录态文件）
//!   FileText/Base64  不支持（凭证是令牌，不是文件）
//! ```
//! 网页登录（设备授权）由 `oauth::login_start` / `login_poll` 走另一条路，
//! 收尾时调 [`account_of`]（与手工粘贴共用同一份落库形态）。
//!
//! ── 站点由 provider 身份决定（不是 payload）────────────────────
//! 界面上是几张卡片，点哪张就落哪一站；payload 里的 `mode` 只是兼容字段，
//! 不再当权威（两边不一致时以 provider 为准）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{Map, Value};

use agent2api_provider_protocol as proto;

use super::auth;
use super::credentials::Credentials;
use super::endpoints::Site;

/// 账号名长度上限
const MAX_NAME_LENGTH: usize = 64;

/// 添加账号入口。
pub(super) async fn normalize(
    site: Site,
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, String> {
    let payload = match input.source {
        proto::NormalizeSource::Manual { payload } => payload,
        proto::NormalizeSource::Desktop => {
            return Err(format!("{}请使用网页登录添加", site.display_name()))
        }
        proto::NormalizeSource::FileText { .. } | proto::NormalizeSource::FileBase64 { .. } => {
            return Err(format!(
                "{}没有可导入的凭证文件，请用网页登录或粘贴账号记录",
                site.display_name()
            ))
        }
    };
    let credentials = auth::prepare_account(&payload, site)
        .await
        .map_err(|error| error.message)?;
    let name = input
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| crate::json::truncate_text(value, MAX_NAME_LENGTH));
    Ok(account_of(site, &credentials, "manual", name))
}

/// 凭证 → 可落库的账号（网页登录与手工粘贴两条路共用）。
///
/// ── 记录字段 ────────────────────────────────────────────────
/// `Credentials::to_value()` 产出的就是落库形态（`accessToken` / `refreshToken`
/// / `mode` / `userId` / `machineId` / `expiresAt` …），所以这里直接用它。
pub(super) fn account_of(
    site: Site,
    credentials: &Credentials,
    source: &str,
    explicit_name: Option<String>,
) -> proto::NormalizedAccount {
    let mut secret = match credentials.to_value() {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    // 站点以 provider 身份为准（payload / 凭证里的 mode 只是兼容字段）
    secret.insert("mode".to_string(), Value::String(site.id().to_string()));
    secret.insert("source".to_string(), Value::String(source.to_string()));
    let name = explicit_name
        .or_else(|| {
            let upstream = credentials.name.trim();
            if upstream.is_empty() { None } else { Some(upstream.to_string()) }
        })
        .unwrap_or_else(|| default_name(site, credentials));
    secret.insert("name".to_string(), Value::String(name));
    let secret = Value::Object(secret);
    let public = account_view(site, &secret);
    let identity = identity_of(site, &secret);
    proto::NormalizedAccount {
        secret,
        public,
        identity,
        desktop: false,
    }
}

/// 没给名字时怎么命名。
///
/// 千问办公的账号上下文里通常没有昵称（`name` 多为空），它自己的客户端是按
/// **邮箱**认号的；拿不到邮箱才退到 uid。
fn default_name(site: Site, credentials: &Credentials) -> String {
    if site.is_qwenwork() {
        if !credentials.email.trim().is_empty() {
            return crate::json::truncate_text(credentials.email.trim(), MAX_NAME_LENGTH);
        }
        if !credentials.user_id.is_empty() {
            return format!("{} {}", site.display_name(), credentials.user_id);
        }
        return site.display_name();
    }
    if credentials.user_id.is_empty() {
        site.display_name()
    } else {
        format!("{} {}", site.product(), credentials.user_id)
    }
}

/// 账号公开视图（界面账号页读的那几个键）。
///
/// **绝不能含 accessToken / refreshToken 本体** —— 只留令牌尾号与
/// 「有没有令牌」两个布尔。这条是安全边界。
pub(super) fn account_view(site: Site, secret: &Value) -> Value {
    let access_token = crate::store::pick(secret, &["accessToken", "access_token", "token"]);
    let user_id = crate::store::pick(secret, &["userId", "user_id"]);
    let name = {
        let explicit = crate::store::pick(secret, &["name"]);
        if !explicit.is_empty() {
            explicit
        } else if !user_id.is_empty() {
            format!("{} {}", site.product(), user_id)
        } else {
            site.display_name()
        }
    };
    let email = crate::store::pick(secret, &["email"]);
    let mut view = serde_json::json!({
        "name": name,
        "uid": user_id,
        "tokenTail": crate::store::token_tail(&access_token),
        // 站点身份（徽章显示用；值就是落盘的 provider id）
        "edition": site.provider_id(),
        "editionLabel": site.label(),
        "source": crate::store::pick(secret, &["source"]),
        "hasSession": !access_token.is_empty(),
        // 有刷新令牌 = 能续期（网页登录与 PAT 都带）
        "canRefresh": !crate::store::pick(secret, &["refreshToken", "refresh_token"]).is_empty(),
        "desktop": false,
    });
    if !email.is_empty() {
        if let Some(object) = view.as_object_mut() {
            object.insert("email".to_string(), Value::String(email));
        }
    }
    let expires_at = secret
        .get("expiresAt")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0);
    if let Some(expires_at) = expires_at {
        if let Some(object) = view.as_object_mut() {
            object.insert("expiresAt".to_string(), Value::from(expires_at));
        }
    }
    view
}

/// 账号的稳定身份（没有 userId 时返回空串让网关按凭证去重）。
///
/// 前缀按产品分：`qoder-<uid>` / `qwenwork-<uid>`。它是**同 provider 内**的
/// 去重键，跨产品不会相撞；改前缀会让存量账号在下次导入时被当成新账号。
pub(super) fn identity_of(site: Site, secret: &Value) -> String {
    let user_id = crate::store::pick(secret, &["userId", "user_id"]);
    if user_id.is_empty() {
        return String::new();
    }
    let prefix = if site.is_qwenwork() { "qwenwork" } else { "qoder" };
    format!("{prefix}-{user_id}")
}
