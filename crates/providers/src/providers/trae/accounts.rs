//! Trae 账号面：归一化 + 公开视图 + 稳定身份。
//!
//! ── 三条添加路径（对应网关 `api::accounts` 的 trae 分支）─────────
//! ```text
//!   Manual{payload}  粘贴凭据（平铺或 CPA 的 {auth{}, account{}} 嵌套形态）
//!                    → 缺 uid 时问一次 GetUserInfo（best-effort）
//!   Desktop          不支持（登录态在客户端自己的加密存储里）
//!   FileText/Base64  不支持（凭证是令牌，不是文件）
//! ```
//! 网页登录是另一条链（`login.rs` → `Session::complete` → 同一个
//! [`account_of`]）。
//!
//! ── 身份怎么认（为什么不能按令牌尾巴去重）──────────────────
//! 去重键是 `(variant, uid)`，uid 由 `profile` 那条优先级决定
//! （GetUserInfo → 回调回显 → 每个 variant 固定的 unknown 名）。
//! 参考实现在这里摔过并被用户报告（v0.12.25）：拿 per-login 的 trace id 兜底
//! 时，同一个账号登两次就落两条记录。所以 [`identity_of`] 也**只由这两项派生**
//! —— 重复登录必然命中同一条记录并覆盖它，而不是堆出第二个账号。
//!
//! ── 空值不覆盖（本文件只负责「产出」，合并规则在网关）──────────
//! 手工粘贴的凭据常常只有一把 accessToken。这时 `refreshToken` / `expiresAt`
//! 之类的空值**不能**写进已有记录（等于把上次登录拿到的续期串洗掉）——
//! 网关的 `plugin_upsert_account` 按 `identity` 命中同一条后做的是字段合并，
//! 而这里给出的 `secret` 就是合并的**新值**。所以 `secret` 里只放**已知**字段：
//! 空字符串字段直接不写（见 [`account_of`]）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。

use serde_json::{Map, Value};

use agent2api_provider_protocol as proto;

use super::credentials::{is_intl_variant, Credential, INTL_UNSUPPORTED};
use super::profile::{self, Identity};

/// 账号名长度上限（与网关 `truncate_chars(&record_name, 100)` 同口径）。
const MAX_NAME_LENGTH: usize = 100;

/// 添加账号入口。
pub(super) async fn normalize(
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, String> {
    let payload = match input.source {
        proto::NormalizeSource::Manual { payload } => payload,
        proto::NormalizeSource::Desktop => {
            return Err(
                "Trae 不支持导入桌面端登录态，请用「网页登录」或粘贴凭证添加账号".to_string(),
            )
        }
        proto::NormalizeSource::FileText { .. } | proto::NormalizeSource::FileBase64 { .. } => {
            return Err(
                "Trae 没有可导入的凭证文件（凭证是令牌），请粘贴 accessToken / 整份 auth 文件内容"
                    .to_string(),
            )
        }
    };
    let mut credential = Credential::from_payload(&payload)?;
    // 身份是 best-effort 的读数（昵称 / uid 只影响显示与去重）：粘贴的凭据
    // 一般不带 uid，这里替用户问一次上游。问不到也照样落账号 ——
    // 「登录已成功却因为读不到昵称而判失败」是本家明确要避免的那种行为。
    if credential.uid.is_empty() {
        if let Ok(identity) = profile::get_user_info(&credential, input.proxy.as_ref()).await {
            credential.uid = identity.uid;
            credential.nickname = identity.nickname;
            credential.enterprise_id = identity.enterprise_id;
        }
    }
    let name = input
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| crate::json::truncate_text(value, MAX_NAME_LENGTH));
    account_of(&credential, "manual", name).into_account().map_err(|error| error)
}

/// 凭据 → 可落库的账号（网页登录与手工粘贴两条路共用）。
///
/// ── 为什么分成 [`AccountDraft`] 再 `.into_account()` ─────────
/// `login.poll` 的返回是 `LoginPoll`（不能报错），而谱系闸门要能拒绝 Intl
/// 凭据。分成两步后：登录链拿 `account_of(...).into_account()` 的 `Err` 变成
/// `LoginPoll::Failed`；`normalize` 直接把 `Err` 透成 400。
pub(super) fn account_of(
    credential: &Credential,
    source: &str,
    explicit_name: Option<String>,
) -> AccountDraft {
    AccountDraft {
        credential: credential.clone(),
        source: source.to_string(),
        explicit_name,
    }
}

/// 待落库的账号（见 [`account_of`] 的说明）。
pub(super) struct AccountDraft {
    credential: Credential,
    source: String,
    explicit_name: Option<String>,
}

impl AccountDraft {
    /// 产出 `NormalizedAccount`（谱系闸门在这里判）。
    pub(super) fn into_account(self) -> Result<proto::NormalizedAccount, String> {
        // 到期读数要在拆字段**之前**算：`effective_expiry_ms()` 读的是整份
        // 结构体（JWT 声明 + 落盘字段），拆完之后拿不到完整的那一份。
        let expires_at_ms = self.credential.effective_expiry_ms();
        let Credential {
            access_token,
            refresh_token,
            domain,
            api_host,
            machine_id,
            device_id,
            variant,
            uid,
            enterprise_id,
            nickname,
            device_public_key,
            device_private_key,
            auth_client_id,
            ..
        } = self.credential;
        // 谱系闸门：Intl 凭据落库后必然在转发时报一句难懂的 401
        // （见 `credentials::is_intl_variant`），所以在**入口**就拒，
        // 而不是收下一条永远不会工作的记录。
        if is_intl_variant(&variant) {
            return Err(INTL_UNSUPPORTED.to_string());
        }
        let normalized_variant = match variant.as_str() {
            "cn" | "solo" | "intl" | "solo-intl" => variant.clone(),
            _ => super::credentials::DEFAULT_VARIANT.to_string(),
        };
        if access_token.trim().is_empty() && refresh_token.trim().is_empty() {
            return Err("Trae 凭据里既没有 accessToken 也没有 refreshToken".to_string());
        }
        let mut secret = Map::new();
        // 只写**已知**字段（见模块头「空值不覆盖」）。expiresAt 单独处理：
        // 落盘统一成毫秒（读取侧 `expires_at_ms()` 两种都认，但面板那一列
        // 按毫秒读 —— 存秒会显示成「1970 年到期」）。
        insert_text(&mut secret, "accessToken", &access_token);
        insert_text(&mut secret, "refreshToken", &refresh_token);
        insert_text(&mut secret, "domain", &domain);
        insert_text(&mut secret, "apiHost", &api_host);
        insert_text(&mut secret, "machineId", &machine_id);
        insert_text(&mut secret, "deviceId", &device_id);
        insert_text(&mut secret, "uid", &uid);
        insert_text(&mut secret, "nickname", &nickname);
        insert_text(&mut secret, "enterpriseId", &enterprise_id);
        insert_text(&mut secret, "devicePublicKey", &device_public_key);
        insert_text(&mut secret, "devicePrivateKey", &device_private_key);
        // 空串落盘会把 CPA 那边迁来的 `authClientId` 抹掉，而那个值一旦没了，
        // 续期就只能按 variant 猜（猜错是 10101「refresh token is not matched
        // to the client」）。
        insert_text(&mut secret, "authClientId", &auth_client_id);
        secret.insert(
            "variant".to_string(),
            Value::String(normalized_variant.clone()),
        );
        if expires_at_ms > 0 {
            secret.insert("expiresAt".to_string(), Value::from(expires_at_ms));
        }
        secret.insert("source".to_string(), Value::String(self.source.clone()));
        // 名字：显式给的 > 上游昵称 > 「Trae <variant>」（与网关
        // `add_trae_account` 的取值顺序一致）。写进 secret 是为了让
        // `account_view` 能读到它（界面读的是 public，而 public 由 secret 派生）。
        let name = self
            .explicit_name
            .or_else(|| Some(nickname.clone()).filter(|value| !value.trim().is_empty()))
            .unwrap_or_else(|| format!("Trae {normalized_variant}"));
        secret.insert("name".to_string(), Value::String(name));
        let secret = Value::Object(secret);
        let public = account_view(&secret);
        let identity = identity_of(&secret);
        Ok(proto::NormalizedAccount {
            secret,
            public,
            identity,
            // 本家没有「实时读本机客户端」这条来源，恒 false
            desktop: false,
        })
    }
}

/// 账号公开视图（界面账号页读的那几个键，与网关
/// `to_trae_public_account` 逐键对应）。
///
/// ── 为什么 `expiresAtMs` 与 `expiresAt` 都给 ────────────────
/// 面板的有效期那一列与别家共用同一套按毫秒的读法（`expiresAtMs`），而
/// `expiresAt` 是落盘字段（历史上有秒也有毫秒）。两个都给，界面读哪个都对；
/// 只给落盘那个的话，秒形态的老记录会显示成 1970 年。
pub(super) fn account_view(secret: &Value) -> Value {
    let access_token = crate::store::pick(secret, &["accessToken"]);
    let refresh_token = crate::store::pick(secret, &["refreshToken"]);
    let uid = crate::store::pick(secret, &["uid"]);
    let nickname = crate::store::pick(secret, &["nickname"]);
    let variant = crate::store::pick(secret, &["variant"]);
    let name = {
        let explicit = crate::store::pick(secret, &["name"]);
        if !explicit.is_empty() {
            explicit
        } else if !nickname.is_empty() {
            nickname.clone()
        } else {
            format!(
                "Trae {}",
                if variant.is_empty() {
                    super::credentials::DEFAULT_VARIANT
                } else {
                    variant.as_str()
                }
            )
        }
    };
    // 到期读数：先按结构体那条链（JWT 声明优先），读不出再退回落盘字段
    let credential = Credential::from_payload(secret).ok();
    let expires_at_ms = credential
        .as_ref()
        .map(Credential::effective_expiry_ms)
        .unwrap_or(0);
    let mut view = serde_json::json!({
        "name": name,
        "uid": uid,
        "nickname": nickname,
        "enterpriseId": crate::store::pick(secret, &["enterpriseId"]),
        "source": crate::store::pick(secret, &["source"]),
        "tokenTail": crate::store::token_tail(&access_token),
        "variant": variant,
        "hasRefreshToken": !refresh_token.trim().is_empty(),
        // 有 accessToken 才算「当前可用」；只有续期串的账号要先刷一次才能用
        "available": !access_token.trim().is_empty(),
        "desktop": false,
    });
    if let Some(object) = view.as_object_mut() {
        if expires_at_ms > 0 {
            object.insert("expiresAtMs".to_string(), Value::from(expires_at_ms));
            object.insert("expiresAt".to_string(), Value::from(expires_at_ms));
        }
    }
    view
}

/// 账号的稳定身份：`trae-<variant>-<uid>`。
///
/// ── 没有 uid 时为什么退到令牌指纹 ───────────────────────────
/// 不给随机 id（那会造出重复账号，见模块头）。登录链路里
/// `Identity::merged` 已经保证了 unknown 兜底；手工粘贴如果真连 uid 都没有，
/// 按 accessToken 派生 —— 同一串凭证重复粘贴仍然命中同一条记录。
pub(super) fn identity_of(secret: &Value) -> String {
    let variant = {
        let raw = crate::store::pick(secret, &["variant"]);
        if raw.is_empty() {
            super::credentials::DEFAULT_VARIANT.to_string()
        } else {
            raw
        }
    };
    let uid = crate::store::pick(secret, &["uid"]);
    if !uid.is_empty() {
        return format!("trae-{variant}-{uid}");
    }
    let access_token = crate::store::pick(secret, &["accessToken"]);
    if access_token.is_empty() {
        return String::new();
    }
    format!("trae-{variant}-token-{access_token}")
}

/// 写一个非空字符串字段（空串 = 没给，见模块头）。
fn insert_text(map: &mut Map<String, Value>, key: &str, value: &str) {
    let trimmed = value.trim();
    if !trimmed.is_empty() {
        map.insert(key.to_string(), Value::String(trimmed.to_string()));
    }
}

/// 从 `Identity` 直接产账号（`login.rs` 用不到，留着给将来的复用点）。
#[allow(dead_code)]
pub(super) fn identity_uid(identity: &Identity) -> String {
    identity.uid.clone()
}
