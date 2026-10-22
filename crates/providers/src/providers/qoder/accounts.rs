//! Qoder 账号面：PAT 兑换 / 设备授权收尾 + 归一化 + 公开视图。
//!
//! ── 两条添加路径（对应网关 `api::accounts` 的 qoder 分支）────────
//! ```text
//!   Manual{payload}  粘贴 PAT（个人访问令牌）→ 换令牌 + 拉资料 → 落账号
//!   Desktop          不支持（Qoder 没有客户端登录态文件）
//!   FileText/Base64  不支持（凭证是令牌，不是文件）
//! ```
//! 网页登录（设备授权）由 `oauth::login_start` / `login_poll` 走另一条路，
//! 收尾时调 [`account_of`]（与 PAT 路径共用同一份落库形态）。
//!
//! ── 地区由 provider 身份决定（不是 payload）────────────────────
//! 拆家后界面上是两张卡片，点哪张就落哪一站；payload 里的 `mode` 只是兼容
//! 字段，不再当权威（两边不一致时以 provider 为准 —— 与网关
//! `add_qoder_account` 同一条规则）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{Map, Value};

use agent2api_provider_protocol as proto;

use super::auth;
use super::credentials::Credentials;
use super::endpoints::Region;

/// 账号名长度上限
const MAX_NAME_LENGTH: usize = 64;

/// 添加账号入口。
pub(super) async fn normalize(
    region: Region,
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, String> {
    let payload = match input.source {
        proto::NormalizeSource::Manual { payload } => payload,
        proto::NormalizeSource::Desktop => {
            return Err(format!(
                "Qoder {}请使用网页登录或个人访问令牌（PAT）添加",
                region.label()
            ))
        }
        proto::NormalizeSource::FileText { .. } | proto::NormalizeSource::FileBase64 { .. } => {
            return Err(format!(
                "Qoder {}没有可导入的凭证文件（凭证是个人访问令牌），请粘贴 PAT 添加",
                region.label()
            ))
        }
    };
    let credentials = auth::prepare_account(&payload, region)
        .await
        .map_err(|error| error.message)?;
    let name = input
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| crate::json::truncate_text(value, MAX_NAME_LENGTH));
    Ok(account_of(region, &credentials, "manual", name))
}

/// 凭证 → 可落库的账号（PAT 与设备授权两条路共用）。
///
/// ── 记录结构与网关 `add_qoder_account` 的字段逐一对应 ─────────
/// `Credentials::to_value()` 产出的就是网关那份记录的形态（`accessToken` /
/// `refreshToken` / `mode` / `userId` / `machineId` / `expiresAt` …），所以
/// 这里直接用它，`refresh::snapshot` 的 `from_payload` 能逐字复用。
pub(super) fn account_of(
    region: Region,
    credentials: &Credentials,
    source: &str,
    explicit_name: Option<String>,
) -> proto::NormalizedAccount {
    let mut secret = match credentials.to_value() {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    // 地区以 provider 身份为准（payload / 凭证里的 mode 只是兼容字段）
    secret.insert("mode".to_string(), Value::String(region.id().to_string()));
    secret.insert("source".to_string(), Value::String(source.to_string()));
    let upstream_name = {
        let name = credentials.name.trim();
        if name.is_empty() {
            None
        } else {
            Some(name.to_string())
        }
    };
    let name = explicit_name
        .or(upstream_name)
        .unwrap_or_else(|| {
            if credentials.user_id.is_empty() {
                format!("Qoder {}", region.label())
            } else {
                format!("Qoder {}", credentials.user_id)
            }
        });
    secret.insert("name".to_string(), Value::String(name));
    let secret = Value::Object(secret);
    let public = account_view(region, &secret);
    let identity = identity_of(&secret);
    proto::NormalizedAccount {
        secret,
        public,
        identity,
        desktop: false,
    }
}

/// 账号公开视图（界面账号页读的那几个键，与网关
/// `to_qoder_public_account` 逐键对应）。
pub(super) fn account_view(region: Region, secret: &Value) -> Value {
    let access_token = crate::store::pick(secret, &["accessToken", "access_token", "token"]);
    let user_id = crate::store::pick(secret, &["userId", "user_id"]);
    let name = {
        let explicit = crate::store::pick(secret, &["name"]);
        if !explicit.is_empty() {
            explicit
        } else if !user_id.is_empty() {
            format!("Qoder {}", user_id)
        } else {
            format!("Qoder {}", region.label())
        }
    };
    let email = crate::store::pick(secret, &["email"]);
    let mut view = serde_json::json!({
        "name": name,
        "uid": user_id,
        "tokenTail": crate::store::token_tail(&access_token),
        "edition": region.provider_id(),
        "editionLabel": region.label(),
        "source": crate::store::pick(secret, &["source"]),
        "hasSession": !access_token.is_empty(),
        // 有刷新令牌 = 能续期（PAT 与设备授权都带）
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

/// 账号的稳定身份：`qoder-<userId>`（没有 userId 时返回空串让网关按凭证去重）。
pub(super) fn identity_of(secret: &Value) -> String {
    let user_id = crate::store::pick(secret, &["userId", "user_id"]);
    if user_id.is_empty() {
        return String::new();
    }
    format!("qoder-{user_id}")
}
