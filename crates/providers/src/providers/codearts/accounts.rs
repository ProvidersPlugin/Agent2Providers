//! CodeArts 账号面：归一化 + 公开视图 + 稳定身份。
//!
//! ── 两条添加路径（对应网关 `api::accounts` 的 codearts 分支）─────
//! ```text
//!   Manual{payload}  粘贴凭证（AK/SK/STS + domain/user + 可选的 refresh token
//!                    与 oauth_context；平铺或 CPA 的 {codearts_provider_credential:{}}
//!                    嵌套形态都认）
//!   Desktop          不支持（登录态是 OAuth 换来的临时三元组，不在本机文件里）
//!   FileText/Base64  不支持（凭证不是文件）
//! ```
//! 网页登录是另一条链（`login.rs` → `oauth::exchange_for` → 同一个
//! [`account_of`]）。
//!
//! ── 身份怎么认（为什么是 domain + user）────────────────────
//! 与 Qoder 的「地区 + userId」同理：同一个人重复登录会得到**同一份记录**
//! （凭证就地更新），而不是在列表里堆出两条一样的账号。缺身份（老数据 /
//! 手工粘贴没给全）时退化成「按 access key 尾号」，绝不猜着合并 —— 把两个
//! 账号合成一条会烧掉其中一份一次性刷新令牌。
//!
//! ── 空值不覆盖（本文件只负责「产出」，合并规则在网关）──────────
//! 手工粘贴的凭据常常只有 AK/SK。这时 `refresh_token` / `oauth_context` 之类
//! 的空值**不能**写进已有记录（等于把上次登录拿到的续期材料洗掉）—— 所以
//! [`AccountDraft::into_account`] 只写**已知**字段。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。

use serde_json::{Map, Value};

use agent2api_provider_protocol as proto;

use super::credentials::Credential;
use super::PROVIDER_ID;

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
                "CodeArts 不支持导入桌面端登录态：它的凭证是 OAuth 换来的临时 AK/SK/STS，\
                 请用「网页登录」或粘贴凭据添加账号"
                    .to_string(),
            )
        }
        proto::NormalizeSource::FileText { .. } | proto::NormalizeSource::FileBase64 { .. } => {
            return Err(
                "CodeArts 没有可导入的凭证文件，请粘贴凭据（access_key_id / secret_access_key）\
                 或整份 CPA auth 文件的内容"
                    .to_string(),
            )
        }
    };
    // 解析失败直接透出原因（`from_payload` 的文案已经点名缺什么）。
    // **不调上游**：凭据是登录换来的，添加时没有可交换的授权码；
    // 目录与连通性由刷新链路验（与网关那条分支逐字同口径）。
    let credential = Credential::from_payload(&payload)?;
    let name = input
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| crate::json::truncate_text(value, MAX_NAME_LENGTH));
    account_of(&credential, "manual", name).into_account()
}

#[allow(dead_code)]
/// 待落库的账号（见 [`account_of`] 的说明）。
pub(super) struct AccountDraft {
    credential: Credential,
    source: String,
    explicit_name: Option<String>,
}

impl AccountDraft {
    /// 产出 `NormalizedAccount`。
    pub(super) fn into_account(self) -> Result<proto::NormalizedAccount, String> {
        let Credential {
            access_key_id,
            secret_access_key,
            security_token,
            expires_at,
            domain_id,
            user_id,
            user_name,
            login_type,
            refresh_token,
            oauth_context,
        } = self.credential;
        if access_key_id.trim().is_empty() || secret_access_key.trim().is_empty() {
            return Err("CodeArts 凭据缺少 access_key_id 或 secret_access_key".to_string());
        }
        let mut secret = Map::new();
        // 只写**已知**字段（见模块头「空值不覆盖」）。
        insert_text(&mut secret, "access_key_id", &access_key_id);
        insert_text(&mut secret, "secret_access_key", &secret_access_key);
        insert_text(&mut secret, "security_token", &security_token);
        insert_text(&mut secret, "expires_at", &expires_at);
        insert_text(&mut secret, "domain_id", &domain_id);
        insert_text(&mut secret, "user_id", &user_id);
        insert_text(&mut secret, "user_name", &user_name);
        insert_text(&mut secret, "login_type", &login_type);
        insert_text(&mut secret, "refresh_token", &refresh_token);
        if let Some(context) = oauth_context.as_ref() {
            // `oauth_context` 是续期的一半（PKCE verifier + DPoP 私钥）：
            // 有就整块写，没有就不写 —— 写一个空对象进去会让 `can_refresh`
            // 判成「能续期但没材料」，比缺字段更难查。
            if let Ok(value) = serde_json::to_value(context) {
                if value.is_object() {
                    secret.insert("oauth_context".to_string(), value);
                }
            }
        }
        secret.insert("source".to_string(), Value::String(self.source.clone()));
        // ── 通用键名（与网关 `to_record_value` 逐字同口径）──────────
        // 网关的账号投影与「这个账号能不能用」的判定读的是这些通用键；
        // 插件家的公开视图由插件自己给，但**选路与启用判定**仍在网关侧读
        // `accessToken` / `expiresAt` 这类共享键（见 `store_view` 与
        // `routing`）。不写就会让 codearts 账号在选路时被当成没有 token。
        secret.insert(
            "accessToken".to_string(),
            Value::String(access_key_id.clone()),
        );
        insert_text(&mut secret, "refreshToken", &refresh_token);
        insert_text(&mut secret, "userId", &user_id);
        let expires_at_ms = Credential {
            expires_at: expires_at.clone(),
            ..Credential::default()
        }
        .expires_at_ms();
        secret.insert(
            "expiresAt".to_string(),
            expires_at_ms.map(Value::from).unwrap_or(Value::Null),
        );
        let secret = Value::Object(secret);
        let public = account_view(&secret);
        let identity = identity_of(&secret);
        Ok(proto::NormalizedAccount {
            secret,
            public,
            identity,
            desktop: false,
        })
    }
}

/// 凭据 → 可落库的账号（网页登录与手工粘贴两条路共用）。
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

/// 界面用的公开视图（**绝不带任何凭证字段**）。
///
/// AK/SK/STS/refresh token/DPoP 私钥一个都不出现在这里 —— 这是账号列表接口，
/// 前端与局域网内的浏览器都能读到它。
pub(super) fn account_view(secret: &Value) -> Value {
    let credential = Credential::from_payload(secret).ok();
    let available = credential
        .as_ref()
        .is_some_and(|value| value.valid() && !value.security_token.trim().is_empty());
    let can_refresh = credential.as_ref().is_some_and(Credential::can_refresh);
    let mut public = Map::new();
    public.insert("name".to_string(), Value::String(default_name(secret)));
    insert_text_public(&mut public, "uid", &store_pick(secret, &["user_id", "userId"]));
    insert_text_public(
        &mut public,
        "userName",
        &store_pick(secret, &["user_name"]),
    );
    insert_text_public(
        &mut public,
        "domainId",
        &store_pick(secret, &["domain_id"]),
    );
    insert_text_public(
        &mut public,
        "loginType",
        &store_pick(secret, &["login_type"]),
    );
    public.insert(
        "tokenTail".to_string(),
        Value::String(crate::store::token_tail(&store_pick(
            secret,
            &["access_key_id", "accessToken"],
        ))),
    );
    // 到期时刻统一给毫秒（面板那一列按毫秒读；RFC3339 字符串它解析不了）
    public.insert(
        "expiresAt".to_string(),
        credential
            .as_ref()
            .and_then(Credential::expires_at_ms)
            .map(Value::from)
            .unwrap_or(Value::Null),
    );
    public.insert(
        "accessTokenExpiresAt".to_string(),
        credential.as_ref().and_then(Credential::expires_at_ms).map(Value::from).unwrap_or(Value::Null),
    );
    public.insert("hasAccessToken".to_string(), Value::Bool(available));
    public.insert("hasRefreshToken".to_string(), Value::Bool(can_refresh));
    public.insert(
        "refreshTokenExpiresAt".to_string(),
        credential.as_ref()
            .and_then(|value| super::authorization::expires_at_ms(&value.refresh_token))
            .map(Value::from).unwrap_or(Value::Null),
    );
    let relogin = super::authorization::relogin_error(secret);
    public.insert("needsRelogin".to_string(), Value::Bool(relogin.is_some()));
    public.insert(
        "needsReloginReason".to_string(),
        relogin.map(|error| Value::String(error.message)).unwrap_or(Value::Null),
    );
    public.insert("available".to_string(), Value::Bool(available));
    // 本家没有版本概念（恒 cn），但前端按 `edition` 分组的地方要能读到空值
    public.insert("edition".to_string(), Value::Null);
    public.insert("editionLabel".to_string(), Value::Null);
    Value::Object(public)
}

/// 稳定身份：`domain_id + user_id`；都空时退到 access key 尾号。
///
/// ── 为什么不能退到 access key 本身 ─────────────────────────
/// AK 每次续期都换（临时凭据），拿它当身份等于每刷一次期就在列表里多一条账号。
/// 尾号同理 —— 但它至少稳定在「这一次登录」的生命周期内，且缺身份本来就是
/// 手工粘贴没给全的边角情形（网关那份也是这条退路）。
pub(super) fn identity_of(secret: &Value) -> String {
    let domain = store_pick(secret, &["domain_id"]);
    let user = store_pick(secret, &["user_id", "userId"]);
    if !domain.is_empty() || !user.is_empty() {
        return format!("codearts-{domain}-{user}");
    }
    let tail = crate::store::token_tail(&store_pick(secret, &["access_key_id", "accessToken"]));
    if tail.is_empty() {
        String::new()
    } else {
        format!("codearts-ak-{tail}")
    }
}

/// 默认账号名：`user_name` → `CodeArts <AK 前 8 位>`。
fn default_name(secret: &Value) -> String {
    let user_name = store_pick(secret, &["user_name"]);
    if !user_name.is_empty() {
        return crate::json::truncate_text(&user_name, MAX_NAME_LENGTH);
    }
    let ak = store_pick(secret, &["access_key_id", "accessToken"]);
    format!(
        "CodeArts {}",
        crate::json::truncate_text(&ak, 8)
    )
}

/// 取一个非空字符串（多候选键，宽容读取）。
fn store_pick(secret: &Value, keys: &[&str]) -> String {
    crate::store::pick(secret, keys)
}

/// 只在值非空时写一个字符串键。
fn insert_text(target: &mut Map<String, Value>, key: &str, value: &str) {
    let trimmed = value.trim();
    if !trimmed.is_empty() {
        target.insert(key.to_string(), Value::String(trimmed.to_string()));
    }
}

/// 公开视图里的同类写入（空值不写，界面显示 `—`）。
fn insert_text_public(target: &mut Map<String, Value>, key: &str, value: &str) {
    let trimmed = value.trim();
    if !trimmed.is_empty() {
        target.insert(key.to_string(), Value::String(trimmed.to_string()));
    }
}

/// 本家的 provider id（公开视图与登录链都要用）。
#[allow(dead_code)]
pub(super) const PROVIDER: &str = PROVIDER_ID;
