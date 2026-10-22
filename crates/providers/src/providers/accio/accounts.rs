//! Accio 账号面：归一化 + 公开视图 + 稳定身份。
//!
//! ── 两条添加路径（对应网关 `api::accounts` 的 accio 分支）─────────
//! ```text
//!   Manual{payload}  粘贴凭证（accessToken / refreshToken）→ 查一次 userinfo
//!                    （best-effort）→ 落账号
//!   Desktop          不支持（登录态在客户端自己的加密存储里）
//!   FileText/Base64  不支持（凭证是令牌，不是文件）
//! ```
//! 网页登录是另一条链（`login.rs` → `oauth::exchange_code` → 同一个
//! [`account_of`]）。
//!
//! ── 地区由 provider 身份决定（不是 payload）────────────────────
//! 拆家后界面上是两张卡片，点哪张就落哪一站；payload 里的 `mode` 只是兼容
//! 字段，不再当权威（两边不一致时以 provider 为准）。
//!
//! ── 身份怎么认（为什么不能按令牌尾巴去重）──────────────────
//! Accio 的 token 是**不透明串**（不是 JWT，解不出 userId），所以身份由
//! `/api/auth/userinfo` 查回的 `id` 决定。「粘贴凭证」这条路查一次资料拿到
//! id；查不到时按凭证内容生成 id —— 至少能保证同一串凭证重复添加时命中同
//! 一条记录，不会堆出一串重复账号（网关那份用 `new_request_id()` 兜底，
//! 那会让同一串凭证每次添加都得到不同的 id，靠 accessToken 相等去重兜住；
//! 这里改成直接按 accessToken 派生，语义更强）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。

use serde_json::{Map, Value};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;

use super::auth;
use super::credentials::{self, Credentials};
use super::endpoints::Region;

/// 账号名长度上限
const MAX_NAME_LENGTH: usize = 100;

/// 添加账号入口。
pub(super) async fn normalize(
    ctx: &PluginCtx,
    region: Region,
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, String> {
    let payload = match input.source {
        proto::NormalizeSource::Manual { payload } => payload,
        proto::NormalizeSource::Desktop => {
            return Err(format!(
                "Accio {}不支持导入桌面端登录态，请用「网页登录」或粘贴凭证添加",
                region.label()
            ))
        }
        proto::NormalizeSource::FileText { .. } | proto::NormalizeSource::FileBase64 { .. } => {
            return Err(format!(
                "Accio {}没有可导入的凭证文件（凭证是令牌），请粘贴 accessToken / refreshToken",
                region.label()
            ))
        }
    };
    // 地区以 provider 身份为准：payload 里的 `mode` 只是兼容字段
    let mut credentials = Credentials::from_payload(&payload).map_err(|error| error.message)?;
    credentials.region = region;
    // 身份是 best-effort 的读数（昵称 / 邮箱只影响显示与去重）：粘贴的凭据
    // 一般不带头像资料，这里替用户问一次上游。问不到也照样落账号。
    if credentials.user_id.is_empty() {
        let proxy = match input.proxy.as_ref() {
            Some(proxy) => Some(proxy.clone()),
            None => None,
        };
        if let Ok(profile) = auth::fetch_profile(&credentials.access_token, region, proxy.as_ref()).await {
            auth::apply_profile(&mut credentials, &profile);
        }
    }
    credentials.ensure_identity();
    // `ensure_identity` 只补 device_id；user_id 仍可能为空（资料接口失败），
    // 那时身份由 `identity_of` 按 accessToken 派生（见模块头）。
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
    credentials: &Credentials,
    source: &str,
    explicit_name: Option<String>,
) -> proto::NormalizedAccount {
    let mut secret = match credentials.to_value() {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    // 地区以 provider 身份为准（payload / 凭证里的 mode 只是兼容字段）
    secret.insert(
        "mode".to_string(),
        Value::String(credentials.region.edition().to_string()),
    );
    secret.insert("source".to_string(), Value::String(source.to_string()));
    // 名字：显式给的 > 上游昵称 > 邮箱 > userId > 「Accio <地区>账号」
    let name = explicit_name
        .or_else(|| Some(credentials.name.clone()).filter(|value| !value.is_empty()))
        .or_else(|| Some(credentials.email.clone()).filter(|value| !value.is_empty()))
        .or_else(|| {
            Some(credentials.user_id.clone())
                .filter(|value| !value.is_empty())
                .map(|value| format!("Accio {value}"))
        })
        .unwrap_or_else(|| format!("Accio {}账号", credentials.region.label()));
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

/// 账号公开视图（界面账号页读的那几个键，与网关
/// `to_accio_public_account` 逐键对应）。
pub(super) fn account_view(secret: &Value) -> Value {
    let region = Region::from_payload(secret).ok();
    let access_token = crate::store::pick(secret, &["accessToken", "access_token", "token"]);
    let user_id = crate::store::pick(secret, &["userId", "user_id", "uid"]);
    let can_refresh = !crate::store::pick(secret, &["refreshToken", "refresh_token"]).is_empty();
    let mut view = serde_json::json!({
        "id": crate::store::pick(secret, &["id"]),
        "provider": crate::store::pick(secret, &["provider"]),
        "name": crate::store::pick(secret, &["name"]),
        "userId": user_id,
        "email": crate::store::pick(secret, &["email"]),
        "nickname": crate::store::pick(secret, &["nickname"]),
        "source": crate::store::pick(secret, &["source"]),
        "tokenTail": crate::store::token_tail(&access_token),
        "edition": region.map(|value| value.edition()),
        "editionLabel": region.map(|value| value.label()),
        "hasRefreshToken": can_refresh,
        "desktop": false,
        // 有 accessToken 就算「当前可用」（refreshToken 只决定能不能续期）
        "available": !access_token.trim().is_empty() && region.is_some(),
    });
    let expires_at = ["expiresAt", "expires_at", "expires"]
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

/// 账号的稳定身份。
///
/// ── 为什么是 `<地区>:<userId 或令牌摘要>` ────────────────────
/// 两个地区是两家 provider，id 空间独立 —— 但同一个人在两地各有一个账号时，
/// 带上地区才能保证「同一条记录恒定、不同账号不同」。没有 userId 时按
/// accessToken 派生（不是随机串）：同一串凭证重复粘贴必然命中同一条记录。
pub(super) fn identity_of(secret: &Value) -> String {
    let edition = Region::from_payload(secret)
        .map(|region| region.edition())
        .unwrap_or("intl");
    let user_id = crate::store::pick(secret, &["userId", "user_id", "uid"]);
    if !user_id.is_empty() {
        return format!("accio-{edition}-{user_id}");
    }
    let access_token = crate::store::pick(secret, &["accessToken", "access_token", "token"]);
    if access_token.is_empty() {
        return String::new();
    }
    format!(
        "accio-{edition}-{}",
        crate::flight::fingerprint(&access_token)
    )
}

/// 从账号记录还原凭证（转发与余额两条链共用）。
///
/// `#[allow(dead_code)]`：两条链现在都直接调 `Credentials::from_payload`
/// （`refresh::snapshot` 里那一处是唯一入口）。留着它是为了给「从原始记录
/// 直接取凭证」这类调用点一个具名入口，避免第三处再写一遍取值链。
#[allow(dead_code)]
pub(super) fn credentials_of(record: &Value) -> Result<Credentials, GatewayError> {
    credentials::Credentials::from_payload(record)
}
