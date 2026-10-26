//! Loomy 凭证：**只有一条来源** —— 账号记录里的登录 session。
//!
//! ── 与别家最大的不同：没有续期，也没有桌面端导入 ─────────────
//! 客户端实测（`electron/xfyun/account-service.js` + 全包检索）：
//!   - 登录接口的 `expire` 是 14 天，**没有任何 refresh / renew 接口**；
//!     过期（业务码 `020002` / `100002`）只能重新走短信登录。
//!   - 登录态只存在于客户端自己的存储里（Electron 会话 / 加密区），
//!     没有 `auth.json` 那种稳定可读的文件形态 —— 因此不提供「导入桌面端
//!     登录态」入口（与 Accio / ZCode 同一处境）。
//!
//! ── 到期时刻：**不估算，只认上游的拒绝** ─────────────────────
//! 登录响应只有 `{session, userid, phone}`，没有到期时刻。曾经按「登录时刻 +
//! 14 天」估一个 `expiresAt` 写进记录，界面上当成有效期展示 —— 结果是账号用满
//! 两周就红着写「已过期」，而上游照常放行（实测：估算到期后仍能签到、能转发）。
//! 估算值不是事实，所以现在**不再写、也不再展示**它。
//!
//! 真正的失效判据只有一个：上游回 `020002` / `100002`（或 HTTP 401/403）。
//! 那时网关会把该账号标记成「需要重新登录」（`needsRelogin`），用户重新登录
//! 成功后标记自动清掉。在此之前，session 一律按「可用」对待。
//!
//! ── 插件形态下的凭证 ────────────────────────────────────────
//! 凭证不再是账号记录的顶层字段，而是 `ctx.store_get` 拿到的**不透明 JSON**
//! （`secret`）：
//! ```json
//! { "session": "…", "userId": "…", "phone": "…", "addedAt": 1760000000000 }
//! ```
//! 形状由本模块自己定义、自己解释，网关不解析。
//!
//! 老记录上可能还留着历史估算的 `expiresAt`：本模块**读它但不据此判失效**
//! （见 [`Credentials::from_secret`] 的说明），界面也不再展示它。

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{PluginCtx, StoreAccount};
use serde_json::{json, Value};

/// 一条 Loomy 登录态。
#[derive(Clone, Debug)]
pub struct Credentials {
    /// 账号 id（`store_get` 回来的那条记录的 id）。
    pub id: String,
    /// 登录 session（**同时是**模型网关的 `token`）
    pub session: String,
    /// 讯飞侧 userid（转发链不读，但台账/日志用它认人）
    #[allow(dead_code)]
    pub userid: String,
    /// 手机号（同上）
    #[allow(dead_code)]
    pub phone: String,
}

/// 从一条取值链里挑第一个非空字符串（凭证可能被手工编辑过，宽容读取）
fn pick<'a>(record: &'a Value, keys: &[&str]) -> String {
    for key in keys {
        let value = record
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !value.is_empty() {
            return value.to_string();
        }
    }
    String::new()
}

/// 从 `secret` 解析凭证（**唯一入口**）。
///
/// 字段兼容：`session`（本插件落盘名）/ `accessToken` / `token`（粘贴形态）。
///
/// 记录上的 `expiresAt` **一律不读**：老记录里那份是历史估算值（登录时刻 +
/// 14 天），不是上游给的事实，按它判失效会误杀仍然可用的账号。失效只认上游
/// 的 `020002` / `100002`（见模块头）。
pub fn from_secret(account: &StoreAccount) -> Result<Credentials, String> {
    let secret = &account.secret;
    let session = pick(secret, &["session", "accessToken", "token"]);
    if session.is_empty() {
        return Err("Loomy 账号缺少 session，请重新登录或粘贴凭证".to_string());
    }
    let userid = pick(secret, &["userId", "userid", "user_id"]);
    let phone = pick(secret, &["phone"]);
    Ok(Credentials {
        id: account.id.clone(),
        session,
        userid,
        phone,
    })
}

/// 取一条账号的凭证（`account` 空串 = 本家队首账号）。
pub async fn snapshot(ctx: &PluginCtx, account: &str) -> Result<Credentials, String> {
    let found = ctx
        .store_get(super::ID, account)
        .await
        .map_err(|error| format!("读取 Loomy 账号失败：{error}"))?;
    from_secret(&found)
}

/// 手机号脱敏（保留前 3 后 4）。
///
/// ── 为什么按 `char` 而不按字节切 ────────────────────────────────
/// `phone` 是**请求体原样**进来的（只 trim + 按字符数截断，不校验格式），
/// 所以它完全可能不是 11 位 ASCII 数字。按字节回切会落在多字节字符中间 ——
/// 一次 panic 就会打断整个插件进程（虽然不带走网关）。按字符取则与内容无关
/// 地安全。
pub fn mask_phone(phone: &str) -> String {
    let chars: Vec<char> = phone.chars().collect();
    if chars.len() < 7 {
        return phone.to_string();
    }
    let head: String = chars[..3].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}****{tail}")
}

/// session 尾 4 字符（公开形态的展示字段）
pub fn session_tail(session: &str) -> String {
    let chars: Vec<char> = session.chars().collect();
    let start = chars.len().saturating_sub(4);
    chars[start..].iter().collect()
}

/// 截断到 `max` 个**字符**（不是字节；昵称可能有中文）
fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    value.chars().take(max).collect()
}

/// 规范化一条账号（手填 / 文件 / 桌面端）。
///
/// ── 身份从哪来（去重靠它）──────────────────────────────────
/// 优先 `userId`（稳定），没有就退回手机号。两者都没有时**不是**「用凭证兜底」
/// 而是明确报错：没有可辨认身份的账号在界面上会是两条长得一样的记录。
///
/// ── 为什么 normalize 阶段不做探活 ──────────────────────────
/// 上游没有「查当前登录态」的只读接口（GetUserInfo 那条是 CGI 的，Loomy 没有）。
/// 唯一能验证 session 的方式是调一次业务接口 —— 那会真的消耗额度 / 触发每日
/// 积分刷新。所以这里只做本地校验（非空、长度），可用性由第一次转发如实报出。
pub async fn normalize(
    _ctx: &PluginCtx,
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, String> {
    let (session, user_id, phone, payload_name) = match &input.source {
        proto::NormalizeSource::Manual { payload } => (
            pick(payload, &["session", "accessToken", "token"]),
            pick(payload, &["userId", "userid", "user_id"]),
            pick(payload, &["phone"]),
            payload
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
        ),
        proto::NormalizeSource::FileText { content, .. } => {
            let text = content.trim();
            // 文本文件既可能是裸 session，也可能是一份 JSON
            match serde_json::from_str::<Value>(text) {
                Ok(parsed) if parsed.is_object() => (
                    pick(&parsed, &["session", "accessToken", "token"]),
                    pick(&parsed, &["userId", "userid", "user_id"]),
                    pick(&parsed, &["phone"]),
                    None,
                ),
                _ => (text.to_string(), String::new(), String::new(), None),
            }
        }
        proto::NormalizeSource::FileBase64 { .. } => {
            return Err("Loomy 不支持二进制文件导入，请粘贴 session".to_string())
        }
        proto::NormalizeSource::Desktop => {
            return Err(
                "Loomy 不支持导入桌面端登录态（登录态不在可读文件里），请用手机号登录或粘贴 session"
                    .to_string(),
            )
        }
    };
    let session = session.trim().to_string();
    if session.is_empty() {
        return Err("缺少 session（session / accessToken / token）".to_string());
    }
    if session.chars().count() > 8192 {
        return Err("session 过长（超过 8192 字符，多半是粘错了内容）".to_string());
    }
    let user_id = truncate_chars(&user_id, 256);
    let phone = truncate_chars(&phone, 256);
    if user_id.is_empty() && phone.is_empty() {
        return Err("缺少 userId 或手机号：至少给一个才能生成可辨认的账号身份".to_string());
    }
    let identity_seed = if user_id.is_empty() { phone.clone() } else { user_id.clone() };
    let identity = format!("loomy-{identity_seed}");
    let display = input
        .name
        .clone()
        .or(payload_name)
        .unwrap_or_else(|| default_name(&phone, &user_id));
    Ok(proto::NormalizedAccount {
        secret: json!({
            "session": session,
            "userId": user_id,
            "phone": phone,
            // 不写 `expiresAt`：上游不给到期时刻，估算值不是事实（见模块头）
            "addedAt": crate::http::now_ms(),
        }),
        public: json!({
            "name": display,
            "uid": user_id,
            "phone": phone,
            "tokenTail": session_tail(&session),
        }),
        identity,
        desktop: false,
    })
}

/// 默认展示名：脱敏手机号 → userId → 通用名
pub fn default_name(phone: &str, user_id: &str) -> String {
    if phone.chars().count() >= 11 {
        format!("Loomy {}", mask_phone(phone))
    } else if !user_id.is_empty() {
        format!("Loomy {user_id}")
    } else {
        "Loomy 账号".to_string()
    }
}

/// 账号的稳定身份（去重用）。
pub fn identity_of(secret: &Value) -> String {
    let user_id = pick(secret, &["userId", "userid", "user_id"]);
    if !user_id.is_empty() {
        return format!("loomy-{user_id}");
    }
    let phone = pick(secret, &["phone"]);
    if !phone.is_empty() {
        return format!("loomy-{phone}");
    }
    let session = pick(secret, &["session", "accessToken", "token"]);
    format!("loomy-{}", session_tail(&session))
}

/// 账号的公开视图（凭证更新后刷新界面用）。
///
/// 不输出 `expiresAt`：老记录里那份是历史估算值（登录时刻 + 14 天），拿它当
/// 有效期展示会让仍然可用的账号红着写「已过期」。失效由网关的 `needsRelogin`
/// 表达（上游回 `020002` / `100002` 时才落，见模块头）。
pub fn account_view(secret: &Value) -> Result<Value, String> {
    let session = pick(secret, &["session", "accessToken", "token"]);
    if session.is_empty() {
        return Err("Loomy 凭证里没有 session".to_string());
    }
    let user_id = pick(secret, &["userId", "userid", "user_id"]);
    let phone = pick(secret, &["phone"]);
    Ok(json!({
        "name": default_name(&phone, &user_id),
        "uid": user_id,
        "phone": phone,
        "tokenTail": session_tail(&session),
    }))
}
