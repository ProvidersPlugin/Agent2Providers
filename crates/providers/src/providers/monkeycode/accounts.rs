//! MonkeyCode 账号面：**粘贴 session** 校验 + 归一化 + 落库形态。
//!
//! ── 链路（一步校验 + 一次 best-effort 发现）────────────────────
//! ```text
//! GET {站点}/api/v1/users/status   Cookie: monkeycode_ai_session=…
//!   → {"code":0,"data":{"user":{"id":"uuid","subscription_level":"pro"}}}
//! GET {站点}/api/v1/users/tasks?page=1&size=5
//!   → data.tasks[].image.id           （best-effort 发现 image_id）
//! ```
//!
//! ── 为什么 image_id 在这里发现而不是要求用户填 ──────────────────
//! 参考 `discoverImageId` 从**已有任务列表**的 `task.image.id` 里取镜像 UUID。
//! 老用户一般至少有一个历史任务，能自动拿到；新用户取不到 —— 此时**不阻断
//! 添加**，把 `imageId` 留空，由界面提示手动填写（转发入口 `chat.rs` 会给出
//! 「缺少 image_id」的可读错误）。这比在添加时就拒绝一个「登录态明明有效」
//! 的账号友好得多。
//!
//! ── 没有桌面端导入 / 没有文件导入 ─────────────────────────────
//! 本家的登录态就是浏览器 cookie，没有客户端 `auth.json` 那种可读文件
//! （与 Accio / ZCode 同一处境）。`Desktop` 与 `FileBase64` / `FileText`
//! 三种来源都要给出可读的回绝理由，而不是静默失败。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{Map, Value};

use agent2api_provider_protocol as proto;

use super::credentials::SESSION_TTL_MS;
use super::login;
use super::region::Region;

/// 账号名长度上限
const MAX_NAME_LENGTH: usize = 64;
/// 身份 / 镜像 id 的长度上限（与账号存储的 `MAX_IDENTITY_LENGTH` 同量级）
const MAX_IDENTITY_LENGTH: usize = 128;

/// 添加账号入口。
pub(super) async fn normalize(
    region: Region,
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, String> {
    let payload = match input.source {
        proto::NormalizeSource::Manual { payload } => payload,
        proto::NormalizeSource::Desktop => {
            return Err(format!(
                "MonkeyCode {}不支持导入桌面端登录态，请粘贴 session（monkeycode_ai_session）添加账号",
                region.label()
            ))
        }
        proto::NormalizeSource::FileText { .. } | proto::NormalizeSource::FileBase64 { .. } => {
            return Err(format!(
                "MonkeyCode {}没有可导入的凭证文件（登录态是浏览器 cookie），请粘贴 session 添加账号",
                region.label()
            ))
        }
    };
    let session = payload
        .get("session")
        .or_else(|| payload.get("accessToken"))
        .or_else(|| payload.get("token"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if session.is_empty() {
        return Err("请粘贴 MonkeyCode 的 session cookie（monkeycode_ai_session）".to_string());
    }
    let provided_image = payload
        .get("imageId")
        .or_else(|| payload.get("image_id"))
        .and_then(Value::as_str);
    // 真实校验：`GET /users/status`（参考实现与文档都把它列为权威端点）
    let verified = login::verify_session(region, &session, provided_image)
        .await
        .map_err(|error| error.message)?;
    let name = input
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| crate::json::truncate_text(value, MAX_NAME_LENGTH));
    Ok(account_of(region, &verified, "manual", name))
}

/// 校验后的凭证 → 可落库的账号。
///
/// ── 记录结构与网关 `add_monkeycode_account` 的字段逐一对应 ─────
/// `accessToken`（session）/ `userId` / `imageId` / `tokenTail` / `expiresAt`
/// / `source` / `name`。这样 `credentials::from_record` 能逐字复用（它读的
/// 就是这几个键），账号页的公开视图也不变。
fn account_of(
    region: Region,
    verified: &Value,
    source: &str,
    explicit_name: Option<String>,
) -> proto::NormalizedAccount {
    let session = text_of(verified, &["session"]);
    let user_id = crate::json::truncate_text(&text_of(verified, &["userId"]), MAX_IDENTITY_LENGTH);
    let image_id = crate::json::truncate_text(&text_of(verified, &["imageId"]), MAX_IDENTITY_LENGTH);
    let upstream_name = text_of(verified, &["name"]);
    let name = explicit_name
        .or_else(|| {
            if upstream_name.is_empty() {
                None
            } else {
                Some(upstream_name)
            }
        })
        .unwrap_or_else(|| {
            if user_id.is_empty() {
                "MonkeyCode 账号".to_string()
            } else {
                format!("MonkeyCode {user_id}")
            }
        });
    let now = crate::http::now_ms();
    let mut secret = Map::new();
    secret.insert("accessToken".to_string(), Value::String(session.clone()));
    secret.insert("tokenTail".to_string(), Value::String(token_tail(&session)));
    secret.insert(
        "expiresAt".to_string(),
        Value::from(now + SESSION_TTL_MS),
    );
    secret.insert("source".to_string(), Value::String(source.to_string()));
    secret.insert("name".to_string(), Value::String(name.clone()));
    if !user_id.is_empty() {
        secret.insert("userId".to_string(), Value::String(user_id.clone()));
    }
    // imageId 空值**不写**：重新粘贴一次 session 不该把上次发现的 image_id
    // 洗掉（与网关 `add_monkeycode_account` 同一条规则）
    if !image_id.is_empty() {
        secret.insert("imageId".to_string(), Value::String(image_id.clone()));
    }
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

/// 取一个非空字符串（多候选键）。
fn text_of(value: &Value, keys: &[&str]) -> String {
    for key in keys {
        let text = value
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !text.is_empty() {
            return text.to_string();
        }
    }
    String::new()
}

/// 令牌尾 8 字符（展示用）。
fn token_tail(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let start = chars.len().saturating_sub(8);
    chars[start..].iter().collect()
}

/// 账号公开视图（界面账号页读的那几个键，与网关
/// `to_monkeycode_public_account` 逐键对应）。
pub(super) fn account_view(region: Region, secret: &Value) -> Value {
    let session = crate::store::pick(secret, &["accessToken", "session", "token"]);
    let user_id = crate::store::pick(secret, &["userId", "user_id"]);
    let image_id = crate::store::pick(secret, &["imageId", "image_id"]);
    let name = {
        let explicit = crate::store::pick(secret, &["name"]);
        if !explicit.is_empty() {
            explicit
        } else if !user_id.is_empty() {
            format!("MonkeyCode {user_id}")
        } else {
            "MonkeyCode 账号".to_string()
        }
    };
    serde_json::json!({
        "name": name,
        "userId": user_id,
        "imageId": if image_id.is_empty() { Value::Null } else { Value::String(image_id.clone()) },
        // 有没有 image_id：没有时界面上「创建任务」注定失败，前端据此提示先补
        "hasImageId": !image_id.is_empty(),
        "tokenTail": crate::store::pick(secret, &["tokenTail"]),
        "edition": region.provider_id(),
        "editionLabel": region.label(),
        "source": crate::store::pick(secret, &["source"]),
        // 本家没有续期协议（30 天硬限制）
        "hasRefreshToken": false,
        "hasSession": !session.is_empty(),
        "desktop": false,
    })
}

/// 账号的稳定身份：`monkeycode-<userId>`（没有 userId 时返回空串让网关按
/// 凭证去重 —— 与网关 `anonymous_account_id` 的「不去重」同一取舍）。
pub(super) fn identity_of(secret: &Value) -> String {
    let user_id = crate::store::pick(secret, &["userId", "user_id"]);
    if user_id.is_empty() {
        return String::new();
    }
    format!("monkeycode-{user_id}")
}
