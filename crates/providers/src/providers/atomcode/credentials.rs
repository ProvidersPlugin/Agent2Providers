//! AtomCode 凭证：**唯一来源**是账号记录里的 OAuth 令牌对。
//!
//! ── 与别家最大的不同：令牌不是 JWT，过期只能算 ──────────────────
//! AtomGit 代理下发的是**不透明随机串**，没有 `exp` 可读（官方 `oauth.rs:143-158`
//! 的 `AuthInfo` 里只有 `created_at` 与 `expires_in` 两个数字）。
//! 所以过期时刻只能 `created_at + expires_in` 推出来 —— 判临期不能像小浣熊
//! 那样解 JWT 的自定义声明。
//!
//! ── 第二条不同：续期**轮换** refresh token ─────────────────────
//! `POST /oauth/refresh` 同时下发新的 access 与新的 refresh，且**旧 access
//! 立即失效**（官方实测：旧令牌随后返回 `401 {"message":"401 Unauthorized"}`）。
//! 含义有两层，两层都要照顾：
//!   - 续期必须把**新的一对**写回记录 —— 丢掉新的 refresh 等于让账号退化成
//!     必须重新走浏览器登录；
//!   - 并发续期会互相作废（第二个写入覆盖掉第一个的新 refresh），
//!     因此续期必须单飞（见 `providers::atomcode::refresh` 与
//!     `core::refresh_flight`）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::Value;

use crate::errors::GatewayError;
use crate::logging;

use super::endpoints::REFRESH_WINDOW_SECONDS;

/// 令牌包（登录与续期共用同一套字段，官方两个端点返回同一个信封）
#[derive(Clone, Debug, Default)]
pub struct TokenBundle {
    /// `access_token`
    pub access_token: String,
    /// `refresh_token`（续期时是**新的**一个）
    pub refresh_token: String,
    /// `token_type`，实测恒 `Bearer`
    pub token_type: String,
    /// `expires_in`（秒）；实测 604800 = 7 天
    pub expires_in: i64,
    /// 身份
    pub user_id: String,
    /// 登录名（如 `qq_23240873`）
    pub username: String,
    /// 显示名（常为中文）
    pub name: String,
    /// 邮箱
    pub email: String,
    /// 头像
    pub avatar_url: String,
}

/// 一次转发 / 查询用的 AtomCode 凭证快照。
#[derive(Clone, Debug)]
pub struct AtomcodeCredentials {
    /// 账号记录 id
    pub id: String,
    /// OAuth 访问令牌（**不透明串**，不是 JWT）
    pub access_token: String,
    /// 轮换用的 refresh token（缺失时只能等用户重登）
    pub refresh_token: String,
    /// 过期时刻（毫秒）。0 = 推不出来（记录被手工编辑过）
    pub expires_at: i64,
    /// AtomGit 用户 id（账号标识与展示）
    ///
    /// 插件侧目前只在续期结果里透传它（展示名与身份由 `mod.rs` 直接从 secret
    /// 的 `userId` 读）—— 留着是因为它是**凭证快照**的一部分：续期合并要把
    /// 上游新给的 `user` 带回来，否则一次「没回 user 的续期」看着像身份丢了。
    #[allow(dead_code)]
    pub user_id: String,
    /// 展示名（备注名优先；同上，续期合并透传用）
    #[allow(dead_code)]
    pub name: String,
}

impl AtomcodeCredentials {
    /// 是否已过期或进入提前续期窗口。
    ///
    /// 窗口取 [`REFRESH_WINDOW_SECONDS`]（1 小时）：比官方「过期才刷」更早，
    /// 因为续期本身会轮换 refresh token —— 提前刷是安全的，而拿死令牌
    /// 发一次请求不是。
    pub fn expiring(&self) -> bool {
        self.expires_at > 0
            && self.expires_at - REFRESH_WINDOW_SECONDS * 1000 <= logging::now_ms()
    }

    /// 已经彻底过期（用于区分「临期」与「必需重登」的提示口径）
    ///
    /// ⚠️ 插件侧暂未消费：现在的提示文案统一由 `expiring()` 一条路给出
    /// （续期失败时的文案已区分「没有 refreshToken → 请重登」）。留着是因为
    /// 它是凭证类型上的一档语义（过期 ≠ 临期），删掉会让将来的判断重新推一遍。
    #[allow(dead_code)]
    pub fn expired(&self) -> bool {
        self.expires_at > 0 && self.expires_at <= logging::now_ms()
    }
}

/// 从一条取值链里挑第一个非空字符串（记录可能被手工编辑过，宽容读取）
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

/// 从一条取值链里挑第一个**正**整数
fn pick_i64(record: &Value, keys: &[&str]) -> i64 {
    for key in keys {
        if let Some(value) = record.get(*key).and_then(Value::as_i64) {
            if value > 0 {
                return value;
            }
        }
    }
    0
}

/// 由 `createdAt + expiresIn` 推出过期时刻（**毫秒**）。
///
/// 这是本家**唯一**的过期来源：令牌不是 JWT，读不到 `exp`。
///
/// ⚠️ **单位是这条链上最容易错的地方**（这里曾经真的错过一次，被三条单测抓住）：
///   - `created_at`：**秒**（代理 `AuthInfo.created_at` 的口径）；
///   - `expires_in`：**秒**（代理 `expires_in` 的口径）；
///   - `fallback_created_at`：**毫秒**（本项目记录里的 `addedAt`，与 `expiresAt` 同单位）；
///   - 返回值：**毫秒**（与别家的 `expiresAt` 同单位）。
///
/// 两种基准都缺、或 `expires_in` 缺失时返回 0 = 判不出来 —— 调用方按
/// 「判不出来就不声称临期」处置（见 [`AtomcodeCredentials::expiring`]），
/// 而不是猜一个值。
pub fn expires_at_from(created_at: i64, expires_in: i64, fallback_created_at: i64) -> i64 {
    if expires_in <= 0 {
        return 0;
    }
    // 秒 → 毫秒；基准缺失时用毫秒形态的兜底值（addedAt）
    let basis_ms = if created_at > 0 {
        created_at * 1000
    } else {
        fallback_created_at
    };
    if basis_ms <= 0 {
        return 0;
    }
    basis_ms + expires_in * 1000
}

/// 从账号记录解析凭证（**唯一入口**；`record` 为 None 时报「没有可用账号」）。
///
/// 字段兼容：`accessToken`（本项目落盘名）/ `access_token`（官方 `auth.toml` 名）/
/// `token`（手工粘贴形态）。
pub fn from_record(record: Option<&Value>) -> Result<AtomcodeCredentials, GatewayError> {
    let Some(record) = record else {
        return Err(GatewayError::with_status(
            401,
            "没有可用的 AtomCode 账号：请在账号页用 AtomGit 浏览器登录添加",
        ));
    };
    let access_token = pick(record, &["accessToken", "access_token", "token"]);
    if access_token.is_empty() {
        return Err(GatewayError::with_status(
            401,
            "AtomCode 账号缺少访问令牌，请重新登录",
        ));
    }
    let refresh_token = pick(record, &["refreshToken", "refresh_token"]);
    let id = pick(record, &["id"]);
    let user_id = pick(record, &["userId", "user_id", "uid"]);
    let name = {
        let note = pick(record, &["name"]);
        if note.is_empty() {
            pick(record, &["username", "userName"])
        } else {
            note
        }
    };
    let explicit_expiry = pick_i64(record, &["expiresAt"]);
    let expires_at = if explicit_expiry > 0 {
        explicit_expiry
    } else {
        expires_at_from(
            pick_i64(record, &["createdAt"]),
            pick_i64(record, &["expiresIn", "expires_in"]),
            pick_i64(record, &["addedAt"]),
        )
    };
    Ok(AtomcodeCredentials {
        id,
        access_token,
        refresh_token,
        expires_at,
        user_id,
        name,
    })
}

/// 账号展示名（用户名优先，其次显示名，最后用户 id）
pub fn display_name(bundle: &TokenBundle) -> String {
    for candidate in [&bundle.username, &bundle.name, &bundle.user_id] {
        let trimmed = candidate.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    "AtomGit 账号".to_string()
}
