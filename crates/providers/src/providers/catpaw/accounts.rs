//! CatPaw 账号面：归一化 + 公开视图 + 稳定身份。
//!
//! ── 三条添加路径（对应网关 `api::accounts` 的 catpaw 分支）─────────
//! ```text
//!   Manual{payload}  三种形态都认（见下）
//!   Desktop          导入本机登录态（~/.meituan-catpaw/auth.json）
//!   FileText        官方 auth.json 或扁平单账号 JSON，解析后复用 Manual 校验
//!   FileBase64      不支持二进制凭证文件
//! ```
//! 网页登录是另一条链（`login.rs` 的双通道 → 同一个 [`account_of`]）。
//!
//! ── Manual 认的三种 payload 形态 ───────────────────────────
//!   1. 扁平：`{ token | accessToken | access_token, uid | userId, loginName?, name? }`；
//!   2. **粘贴原项目的账号记录**（`accounts[i]` 整条）：除上面的字段，还带
//!      `id` / `tokenTail` / `addedAt` —— 少给 uid 时用 `id` 兜底；
//!   3. **粘贴桌面端 auth.json**：`{ auth: { loginType, accessToken },
//!      account: { uid, loginName } }`。
//!
//! ── 校验照抄原项目 `safeString` 的口径 ─────────────────────
//!   - token 非空、≤ `MAX_TOKEN_LENGTH`、**不含 `[\r\n;]`**
//!     （这些值最终进 Cookie / user-uid 头，换行是头注入、分号会截断 Cookie）；
//!   - `auth.loginType` 非空且不是 `passport` → 拒绝；
//!   - uid / loginName 至少有一个（否则这条账号无法标识，也拼不出 user-uid）。
//!
//! ── 身份怎么认 ────────────────────────────────────────────
//! 沿原项目规则：`uid || loginName`（**不加 `user-` 前缀** —— 那是 workbuddy
//! 的 id 形态，两家 id 空间独立且原项目的账号就是按 uid 寻址的）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。

use serde_json::{Map, Value};

use agent2api_provider_protocol as proto;

use super::credentials::DESKTOP_ACCOUNT_ID;

/// 备注名长度上限（原项目 `name.trim().slice(0, 100)`）
const MAX_NAME_LENGTH: usize = 100;

/// uid / loginName 的长度上限（原项目 `MAX_USER_UID_LENGTH = 256`）
const MAX_USER_UID_LENGTH: usize = 256;

/// 余额查询凭证的字段名（与 `balance::BALANCE_TOKEN_FIELD` 同值）。
///
/// 这里不直接引用那个常量：`balance` 是消费方，`accounts` 是生产方，两边
/// 各自声明同一个字面量比互相 import 更能暴露「有人改了其中一个」。
const BALANCE_TOKEN_FIELD: &str = "balanceToken";

/// 添加账号入口。
///
/// ── 为什么是同步的 ──────────────────────────────────────────
/// CatPaw 的三条来源都不出网（粘贴 / 桌面端文件 / 文件导入），所以归一化
/// 本身没有 await 点。做成同步让 `login_poll` 能在**持锁的闭包里**直接调用
/// （那里拿不到 `PluginCtx` 也无法 await），adapter 的 trait 方法再包一层 async。
pub(super) fn normalize(
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, String> {
    let payload = match input.source {
        proto::NormalizeSource::Manual { payload } => payload,
        proto::NormalizeSource::Desktop => {
            // 桌面端：记录里**不落 token**（每次实时读 auth.json），只写引用与展示字段。
            return desktop_account();
        }
        proto::NormalizeSource::FileText { content, .. } => {
            serde_json::from_str::<Value>(&content)
                .map_err(|_| "CatPaw 凭证文件不是合法 JSON，请选择 auth.json 或单账号 JSON 文件".to_string())?
        }
        proto::NormalizeSource::FileBase64 { .. } => {
            return Err("CatPaw 不支持二进制凭证文件，请选择 auth.json 或单账号 JSON 文件".to_string())
        }
    };
    let object = payload
        .as_object()
        .ok_or_else(|| "上传内容必须是 JSON 对象".to_string())?;
    let auth = object_or_empty(object.get("auth"));
    let account = object_or_empty(object.get("account"));
    // 登录方式：auth.json 形态下必查（原项目同）；扁平形态通常没有这个字段
    let login_type = auth
        .get("loginType")
        .or_else(|| object.get("loginType"))
        .map(js_string)
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    if !login_type.is_empty() && login_type != "passport" {
        return Err(format!(
            "暂不支持 CatPaw 登录方式 {login_type}（仅支持 passport）"
        ));
    }
    let token = pick_first_text(&[
        object.get("token"),
        object.get("accessToken"),
        object.get("access_token"),
        object.get("auth_token"),
        auth.get("accessToken"),
        auth.get("token"),
        auth.get("access_token"),
    ])?;
    let uid_raw = pick_first_optional(&[
        object.get("uid"),
        object.get("userId"),
        account.get("uid"),
        account.get("userId"),
        // 原项目的账号记录：id 就是 uid 或 loginName（uid 缺失时才用它兜底）
        object.get("id"),
    ]);
    let login_name = pick_first_optional(&[
        object.get("loginName"),
        account.get("loginName"),
    ]);
    let user_key = if !uid_raw.trim().is_empty() {
        uid_raw.trim().to_string()
    } else {
        login_name.trim().to_string()
    };
    if user_key.is_empty() {
        return Err("CatPaw 登录态缺少 uid 或 loginName（无法标识账号）".to_string());
    }
    let user_key = safe_field(user_key, "uid", MAX_USER_UID_LENGTH)?;
    let uid = if uid_raw.trim().is_empty() {
        String::new()
    } else {
        safe_field(uid_raw, "uid", MAX_USER_UID_LENGTH)?
    };
    let login_name = if login_name.trim().is_empty() {
        String::new()
    } else {
        safe_field(login_name, "loginName", MAX_USER_UID_LENGTH)?
    };
    let balance_token = pick_balance_token(object, &auth);
    let payload_name = pick_first_optional(&[object.get("name"), account.get("name")]);
    let explicit_name = input
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| crate::json::truncate_text(value, MAX_NAME_LENGTH));
    Ok(account_of(
        &user_key,
        &uid,
        &login_name,
        &token,
        balance_token.as_deref(),
        "manual",
        explicit_name.or_else(|| Some(payload_name).filter(|value| !value.is_empty())),
        false,
    ))
}

/// 桌面端实时账号：记录里不落 token，只写 `desktop: true` 与展示字段。
///
/// ── 为什么 id 是固定的 `desktop-auth` ───────────────────────
/// 与原项目逐字一致（不是小浣熊那种 `<provider>-desktop` 形态）：账号 id 是
/// 落进账号文件与前端列表的契约，沿用原值让从旧代理迁移过来的用户一眼能认出
/// 同一条记录。重复导入必然命中同一条，幂等。
fn desktop_account() -> Result<proto::NormalizedAccount, String> {
    let summary = super::credentials::desktop_summary()?;
    let uid = summary
        .get("uid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let login_name = summary
        .get("loginName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let token_tail = summary
        .get("tokenTail")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut secret = Map::new();
    secret.insert(
        "id".to_string(),
        Value::String(DESKTOP_ACCOUNT_ID.to_string()),
    );
    secret.insert("desktop".to_string(), Value::Bool(true));
    secret.insert("source".to_string(), Value::String("desktop".to_string()));
    secret.insert("uid".to_string(), Value::String(uid.clone()));
    secret.insert("loginName".to_string(), Value::String(login_name.clone()));
    secret.insert("tokenTail".to_string(), Value::String(token_tail));
    if let Some(modified) = summary.get("modifiedAt") {
        secret.insert("desktopMtime".to_string(), modified.clone());
    }
    let default_name = if login_name.is_empty() {
        if uid.is_empty() {
            "桌面端登录账号".to_string()
        } else {
            format!("桌面端登录账号（{uid}）")
        }
    } else {
        format!("桌面端登录账号（{login_name}）")
    };
    secret.insert("name".to_string(), Value::String(default_name));
    let secret = Value::Object(secret);
    Ok(proto::NormalizedAccount {
        public: account_view(&secret),
        identity: DESKTOP_ACCOUNT_ID.to_string(),
        secret,
        desktop: true,
    })
}

/// 凭证 → 可落库的账号（网页登录与手工粘贴两条路共用）。
#[allow(clippy::too_many_arguments)]
fn account_of(
    user_key: &str,
    uid: &str,
    login_name: &str,
    token: &str,
    balance_token: Option<&str>,
    source: &str,
    explicit_name: Option<String>,
    desktop: bool,
) -> proto::NormalizedAccount {
    let mut secret = Map::new();
    // id 用 `user_key`（uid 优先、loginName 兜底）—— 与网关 `add_catpaw_account` 一致
    secret.insert("id".to_string(), Value::String(user_key.to_string()));
    secret.insert("uid".to_string(), Value::String(uid.to_string()));
    secret.insert(
        "loginName".to_string(),
        Value::String(login_name.to_string()),
    );
    secret.insert("accessToken".to_string(), Value::String(token.to_string()));
    secret.insert(
        "tokenTail".to_string(),
        Value::String(crate::store::token_tail(token)),
    );
    // 取不到就不出键 ——「未配置」是有意义的状态（前端据此提示去配置），
    // 写一个空串会让「配置了但填错」与「没配置」不可区分。
    if let Some(balance_token) = balance_token {
        secret.insert(
            BALANCE_TOKEN_FIELD.to_string(),
            Value::String(balance_token.to_string()),
        );
    }
    secret.insert("source".to_string(), Value::String(source.to_string()));
    secret.insert("desktop".to_string(), Value::Bool(desktop));
    // 备注名兜底照抄原项目：显式传入 → loginName → uid
    let name = explicit_name
        .or_else(|| Some(login_name.to_string()).filter(|value| !value.is_empty()))
        .or_else(|| Some(uid.to_string()).filter(|value| !value.is_empty()))
        .unwrap_or_else(|| format!("账号 {user_key}"));
    secret.insert(
        "name".to_string(),
        Value::String(crate::json::truncate_text(&name, MAX_NAME_LENGTH)),
    );
    let secret = Value::Object(secret);
    proto::NormalizedAccount {
        public: account_view(&secret),
        identity: identity_of(&secret),
        secret,
        desktop,
    }
}

/// 账号公开视图（界面账号页读的那几个键，与网关
/// `to_catpaw_public_account` 逐键对应）。
///
/// ── 桌面端账号为什么要实时读一次 auth.json ─────────────────
/// 那条记录的凭证不在库里（每次实时读文件），所以 `tokenTail` / `uid` 在库里
/// 是「上次导入时的快照」。用户重新登录后客户端会改写 auth.json —— 不实时读
/// 就会显示一个已经不存在的令牌尾，而且「可用性」也无从判断。
///
/// 读不到时**不报错**：回落记录里的存量值并把 `available` 置 false，
/// 附一句 `reason`（前端据此提示「请先在 CatPaw 桌面端登录」）。
pub(super) fn account_view(secret: &Value) -> Value {
    let desktop = secret
        .get("desktop")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || secret.get("id").and_then(Value::as_str) == Some(DESKTOP_ACCOUNT_ID);
    let mut token_tail = crate::store::pick(secret, &["tokenTail"]);
    let mut uid = crate::store::pick(secret, &["uid"]);
    let mut login_name = crate::store::pick(secret, &["loginName"]);
    let mut available = true;
    let mut reason = String::new();
    if desktop {
        match super::credentials::desktop_summary() {
            Ok(summary) => {
                if let Some(value) = summary.get("tokenTail").and_then(Value::as_str) {
                    token_tail = value.to_string();
                }
                if let Some(value) = summary.get("uid").and_then(Value::as_str) {
                    if !value.is_empty() {
                        uid = value.to_string();
                    }
                }
                if let Some(value) = summary.get("loginName").and_then(Value::as_str) {
                    if !value.is_empty() {
                        login_name = value.to_string();
                    }
                }
            }
            Err(reason_text) => {
                available = false;
                reason = reason_text;
            }
        }
    }
    let mut view = serde_json::json!({
        "name": crate::store::pick(secret, &["name"]),
        "uid": uid,
        "loginName": login_name,
        "tokenTail": token_tail,
        "desktop": desktop,
        "source": crate::store::pick(secret, &["source"]),
        "available": available,
        // 余额凭证**只透出「配没配」**，绝不透出值本身：账号列表是会被截图 /
        // 贴出来排障的界面，而这一项是完整的会话凭证。
        "hasBalanceToken": !crate::store::pick(secret, &[BALANCE_TOKEN_FIELD]).is_empty(),
    });
    if !reason.is_empty() {
        if let Some(object) = view.as_object_mut() {
            object.insert("reason".to_string(), Value::String(reason));
        }
    }
    view
}

/// 账号的稳定身份：`uid || loginName`（原项目 id 规则），桌面端固定 `desktop-auth`。
pub(super) fn identity_of(secret: &Value) -> String {
    if secret.get("id").and_then(Value::as_str) == Some(DESKTOP_ACCOUNT_ID) {
        return DESKTOP_ACCOUNT_ID.to_string();
    }
    let uid = crate::store::pick(secret, &["uid"]);
    if !uid.is_empty() {
        return uid;
    }
    crate::store::pick(secret, &["loginName"])
}

/// 取第一个非空文本（按候选顺序；先去 `Bearer ` 前缀、再去空白）。
///
/// 为什么不像 workbuddy 那样直接用 `pick_token`：那个函数只认顶层键，而
/// CatPaw 的登录态可能是嵌套的 `auth.accessToken` 形态（粘贴 auth.json）或
/// 原项目的 `access_token` 形态 —— 顶层 / 嵌套两种布局都要认。
fn pick_first_text(candidates: &[Option<&Value>]) -> Result<String, String> {
    for value in candidates.iter().flatten() {
        let text = js_string(value);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        return safe_field(strip_bearer_prefix(trimmed), "token", crate::limits::MAX_TOKEN_LENGTH);
    }
    Err("缺少 CatPaw 登录凭证（token / accessToken / access_token）".to_string())
}

/// 取第一个非空文本（不校验，仅用于 uid / loginName / name 这类可选字段）
fn pick_first_optional(candidates: &[Option<&Value>]) -> String {
    for value in candidates.iter().flatten() {
        if value.is_null() {
            continue;
        }
        let text = js_string(value).trim().to_string();
        if !text.is_empty() {
            return text;
        }
    }
    String::new()
}

/// 从添加 payload 里取余额查询凭证（`balanceToken`，可选）。
///
/// 三种键名都认（顺序即优先级）：`balanceToken`（本项目字段名）、`token2`
/// （上游 cookie 名，用户从浏览器复制时看到的多半是这个）、
/// `balanceCookie.token2`（粘贴原项目账号记录时的形态）。
/// 校验失败时**跳过这一个候选**而不是让整条添加失败：余额凭证是可选字段，
/// 填错它不该把「添加账号」这个主流程堵死。
fn pick_balance_token(object: &Map<String, Value>, auth: &Map<String, Value>) -> Option<String> {
    let candidates: Vec<Option<&Value>> = vec![
        object.get("balanceToken"),
        object.get("token2"),
        auth.get("token2"),
        object
            .get("balanceCookie")
            .and_then(|cookie| cookie.get("token2")),
    ];
    for value in candidates.into_iter().flatten() {
        let text = js_string(value).trim().to_string();
        if text.is_empty() {
            continue;
        }
        let trimmed = strip_bearer_prefix(&text);
        if let Ok(safe) = safe_field(trimmed, BALANCE_TOKEN_FIELD, crate::limits::MAX_TOKEN_LENGTH) {
            return Some(safe);
        }
        crate::logging::verbose(
            "[Accounts]",
            "CatPaw 余额查询凭证格式无效（超长或含 CR/LF/分号），已忽略",
        );
    }
    None
}

/// 字段校验（原项目 `safeString`）：非空、不超长、不含 CR/LF/分号。
fn safe_field(value: String, field: &str, max_length: usize) -> Result<String, String> {
    let trimmed = value.trim().to_string();
    if trimmed.is_empty() {
        return Err(format!("CatPaw 登录态缺少 {field}"));
    }
    if trimmed.chars().count() > max_length || trimmed.contains(['\r', '\n', ';']) {
        return Err(format!("CatPaw 登录态中的 {field} 格式无效"));
    }
    Ok(trimmed)
}

/// 值 → 字符串（JS `String(x)`：数字/布尔按字面量、null 给空串）
fn js_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        other => other.to_string(),
    }
}

/// 去 `Bearer ` 前缀（大小写不敏感，只去一次）。
fn strip_bearer_prefix(value: &str) -> String {
    const PREFIX: &str = "bearer";
    if value.len() < PREFIX.len() {
        return value.to_string();
    }
    let head = value.get(..PREFIX.len()).unwrap_or("");
    if !head.eq_ignore_ascii_case(PREFIX) {
        return value.to_string();
    }
    value
        .get(PREFIX.len()..)
        .unwrap_or("")
        .trim_start()
        .to_string()
}

fn object_or_empty(value: Option<&Value>) -> Map<String, Value> {
    match value {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}

