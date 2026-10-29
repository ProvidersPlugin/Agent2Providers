//! WorkBuddy 账号面：手动粘贴 / 官方客户端登录态导入 / 公开视图 / 稳定身份。
//!
//! ── 三条入口（对照网关 `api::accounts` 的 WorkBuddy 分支）────────
//! ```text
//!   Manual{payload}    粘贴嵌套形 auth.json，或 wb2api 那种扁平形
//!   FileText{content}  用户手选的登录态文件（内容与 Desktop 那份同构）
//!   Desktop            读本机官方客户端目录里的当前登录态
//!   FileBase64         不支持 —— 登录态是文本 JSON，明确回执并指路 FileText
//! ```
//!
//! ── 为什么落的是快照而不是「指向本机的引用」────────────────────
//! 协议文档对 `Desktop` 的期望是「不落凭证、转发时实时读」。WorkBuddy 走
//! 快照：官方客户端登录态是**明文 JSON**、路径固定、内容是那一刻的令牌；
//! 网关那份 `add_account` 也是把文件内容原样落库（见
//! `account_store::workbuddy_import` 的模块头）。落快照还有一个实际好处 ——
//! 导入后可以脱离那台客户端（换机、备份恢复）继续用，refresh token 仍能续期。
//!
//! ── 与内置 `add_account` 的字段口径 ──────────────────────────
//! secret 用**扁平**键名（`accessToken` / `uid` / `edition` …）：插件侧
//! `session_of_record` 按扁平读、内置实现按扁平写，两边同名。因此插件接手
//! 存量账号时（`plugin_accounts::flatten_builtin_secret`）字段逐个对得上。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::path::PathBuf;

use serde_json::{Map, Value};

use agent2api_provider_protocol as proto;

use super::region::Region;

/// 令牌长度上限（与账号存储的 `MAX_TOKEN_LENGTH` 同量级）
const MAX_TOKEN_LENGTH: usize = 32_768;
/// 身份字段长度上限
const MAX_IDENTITY_LENGTH: usize = 256;
/// 备注名长度上限
const MAX_NAME_LENGTH: usize = 100;

/// 手动添加：粘贴的 payload → 一条可落库的账号。
pub(super) fn normalize(input: proto::NormalizeInput, region: Region) -> Result<proto::NormalizedAccount, String> {
    let payload = match input.source {
        proto::NormalizeSource::Manual { payload } => payload,
        proto::NormalizeSource::FileText { content, file_name } => {
            let text = content.trim();
            if text.is_empty() {
                return Err(format!(
                    "凭证文件{}是空的",
                    file_name.map(|name| format!("「{name}」")).unwrap_or_default()
                ));
            }
            serde_json::from_str::<Value>(text).map_err(|error| {
                format!(
                    "凭证文件{}不是合法 JSON：{error}（请选择官方客户端登录后生成的 .info 文件）",
                    file_name.map(|name| format!("「{name}」")).unwrap_or_default()
                )
            })?
        }
        proto::NormalizeSource::Desktop => {
            let path = select_desktop_file(region)?;
            let text = std::fs::read_to_string(&path).map_err(|error| {
                format!("读取登录态文件失败（{}）：{error}", path.display())
            })?;
            serde_json::from_str::<Value>(&text).map_err(|error| {
                format!("登录态文件不是合法 JSON（{}）：{error}", path.display())
            })?
        }
        proto::NormalizeSource::FileBase64 { .. } => {
            return Err(
                "WorkBuddy 的登录态是文本 JSON，请用「文件导入」选择 .info 文件（不支持二进制）"
                    .to_string(),
            )
        }
    };
    account_of(&payload, &input.name, region)
}

/// payload（嵌套形或扁平形）→ 账号。字段口径与内置 `add_account` 对齐。
fn account_of(
    payload: &Value,
    explicit_name: &Option<String>,
    region: Region,
) -> Result<proto::NormalizedAccount, String> {
    let Some(object) = payload.as_object() else {
        return Err("账号内容必须是 JSON 对象".to_string());
    };
    let auth = object.get("auth").and_then(Value::as_object);
    let account = object.get("account").and_then(Value::as_object);

    // 嵌套形优先（官方客户端 / wb2api 导出），没有 `auth` 壳再回退顶层
    let access_token = pick_nested(auth, object, &["accessToken", "token", "access_token"]);
    let refresh_token = pick_nested(auth, object, &["refreshToken", "refresh_token"]);
    if access_token.is_empty() {
        return Err("缺少 accessToken".to_string());
    }
    if access_token.chars().count() > MAX_TOKEN_LENGTH
        || refresh_token.chars().count() > MAX_TOKEN_LENGTH
    {
        return Err("token 过长".to_string());
    }
    let uid = {
        let nested = account
            .and_then(|map| map.get("uid"))
            .or_else(|| object.get("uid"));
        truncate(text_of_opt(nested), MAX_IDENTITY_LENGTH)
    };
    if uid.is_empty() {
        return Err("缺少 uid（无法标识账号）".to_string());
    }

    let mut secret = Map::new();
    secret.insert("uid".to_string(), Value::String(uid.clone()));
    secret.insert("accessToken".to_string(), Value::String(access_token.clone()));
    if !refresh_token.is_empty() {
        secret.insert("refreshToken".to_string(), Value::String(refresh_token));
    }
    // 展示与组织字段：`account` 子对象优先，其次顶层（扁平形 wb2api 只给令牌）
    for (key, nested_key) in [
        ("nickname", "nickname"),
        ("type", "type"),
        ("enterpriseId", "enterpriseId"),
        ("enterpriseName", "enterpriseName"),
        ("departmentFullName", "departmentFullName"),
    ] {
        let value = account
            .and_then(|map| map.get(nested_key))
            .or_else(|| object.get(key))
            .map(text_of)
            .unwrap_or_default();
        if !value.is_empty() {
            secret.insert(key.to_string(), Value::String(truncate(value, MAX_IDENTITY_LENGTH)));
        }
    }
    for (key, candidates) in [
        ("expiresAt", ["expiresAt", "expires_at"]),
        ("refreshExpiresAt", ["refreshExpiresAt", "refresh_expires_at"]),
    ] {
        let value = pick_number(auth, object, &candidates);
        if value > 0.0 {
            secret.insert(key.to_string(), serde_json::json!(value));
        }
    }
    let domain = pick_nested(auth, object, &["domain"]);
    if !domain.is_empty() {
        secret.insert("domain".to_string(), Value::String(domain));
    }
    // 端点 / 前缀 / 平台：payload 显式给了就用它（自建网关场景），否则按地区兜底。
    // 三项都**必须**落下来：转发链的 `session_of_record` 从记录读它们，缺了会
    // 退到地区默认值 —— 对自建网关就是打错站点（症状是「刚加的账号不能用」）。
    let edition = region.edition();
    for (key, fallback) in [
        ("endpoint", edition.endpoint),
        ("prefixPath", edition.prefix_path),
        ("platform", edition.platform),
    ] {
        let given = object.get(key).map(text_of).unwrap_or_default();
        secret.insert(
            key.to_string(),
            Value::String(if given.is_empty() { fallback.to_string() } else { given }),
        );
    }
    secret.insert(
        "edition".to_string(),
        Value::String(region.edition_id().to_string()),
    );
    secret.insert(
        "tokenTail".to_string(),
        Value::String(crate::store::token_tail(&access_token)),
    );

    // 名字：显式给的 > 账号昵称 > `账号 <uid 前 8 位>`（与内置同款兜底）
    let nickname = secret
        .get("nickname")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let name = explicit_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| truncate(value.to_string(), MAX_NAME_LENGTH))
        .or_else(|| Some(nickname).filter(|value| !value.is_empty()))
        .unwrap_or_else(|| format!("账号 {}", truncate(uid.clone(), 8)));
    secret.insert("name".to_string(), Value::String(name));

    let secret = Value::Object(secret);
    Ok(proto::NormalizedAccount {
        public: account_view(&secret, region),
        identity: identity_of(&secret),
        secret,
        // 快照形态：凭证落在记录里，没有「转发时实时读」那一层
        desktop: false,
    })
}

/// 公开视图（界面账号卡读的那几个键）。
///
/// 键名与内置 `to_public_account` 对齐 —— 前端按 provider 渲染列，
/// 少一个 `editionLabel` 会让这一家的版本徽章变成空白。
pub(super) fn account_view(secret: &Value, region: Region) -> Value {
    let uid = crate::store::pick(secret, &["uid"]);
    let name = crate::store::pick(secret, &["name"]);
    let nickname = crate::store::pick(secret, &["nickname"]);
    let edition = crate::store::pick(secret, &["edition"]);
    let edition = if edition.is_empty() {
        region.edition()
    } else {
        crate::workbuddy_shared::endpoints::resolve_edition(Some(&edition))
    };
    let mut view = serde_json::json!({
        "uid": uid,
        "name": if name.is_empty() { nickname.clone() } else { name },
        "nickname": nickname,
        "type": crate::store::pick(secret, &["type"]),
        "enterpriseId": crate::store::pick(secret, &["enterpriseId"]),
        "enterpriseName": crate::store::pick(secret, &["enterpriseName"]),
        "tokenTail": crate::store::token_tail(&crate::store::pick(secret, &["accessToken"])),
        "hasRefreshToken": !crate::store::pick(secret, &["refreshToken"]).is_empty(),
        "edition": edition.id,
        "editionLabel": edition.label,
        "endpoint": crate::store::pick(secret, &["endpoint"]),
        "prefixPath": crate::store::pick(secret, &["prefixPath"]),
        "available": !crate::store::pick(secret, &["accessToken"]).is_empty(),
    });
    let expires_at = secret.get("expiresAt").cloned().unwrap_or(Value::Null);
    if let Some(object) = view.as_object_mut() {
        object.insert("expiresAt".to_string(), expires_at);
        object.insert("refreshTokenExpiresAt".to_string(), secret.get("refreshExpiresAt").cloned().unwrap_or(Value::Null));
    }
    view
}

/// 稳定身份：`user-<uid>`（与内置 `add_account` 逐字一致）。
///
/// 口径分叉的代价很具体：同一个账号「重新导入 / 重新粘贴」会被当成新记录，
/// 而内置实现那边是就地更新 —— 用户会看到两份一模一样的账号。
pub(super) fn identity_of(secret: &Value) -> String {
    let uid = crate::store::pick(secret, &["uid"]);
    if uid.is_empty() {
        return String::new();
    }
    format!("user-{uid}")
}

// ─── 官方客户端登录态文件 ────────────────────────────────────

/// 官方客户端凭证目录（Windows）：`%LOCALAPPDATA%\CodeBuddyExtension\…`
fn auth_directory_windows() -> Option<PathBuf> {
    std::env::var("LOCALAPPDATA")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|base| {
            PathBuf::from(base)
                .join("CodeBuddyExtension")
                .join("Data")
                .join("Public")
                .join("auth")
        })
}

/// 官方客户端凭证目录（macOS）：`~/Library/Application Support/CodeBuddyExtension/…`
fn auth_directory_macos() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .filter(|value| !value.trim().is_empty())?;
    Some(
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("CodeBuddyExtension")
            .join("Data")
            .join("Public")
            .join("auth"),
    )
}

/// 当前（不带时间戳）登录态文件名，按地区分。
fn current_file_name(region: Region) -> &'static str {
    match region {
        Region::Cn => "workbuddy-desktop.info",
        Region::Intl => "workbuddy-desktop-ai.info",
    }
}

/// 备份文件前缀（含结尾的点，天然把另一家排除在外）。
fn backup_prefix(region: Region) -> &'static str {
    match region {
        Region::Cn => "workbuddy-desktop.",
        Region::Intl => "workbuddy-desktop-ai.",
    }
}

/// 定位本地区最新的一份登录态文件。
///
/// 客户端每次重新登录都会把上一份改名成
/// `workbuddy-desktop[-ai].<UTC时间戳>.<pid>.<uuid>.info` 留着不删，所以
/// 「当前文件优先、缺失时取时间戳最新的备份」—— 备份里的 accessToken 可能
/// 已过期，但 refreshToken 一般仍在有效期内，落库后由续期链补上。
fn select_desktop_file(region: Region) -> Result<PathBuf, String> {
    let dir = auth_directory_windows()
        .or_else(auth_directory_macos)
        .ok_or_else(|| "无法定位用户目录，找不到官方客户端的登录态文件".to_string())?;
    let current = dir.join(current_file_name(region));
    if current.is_file() {
        return Ok(current);
    }
    let prefix = backup_prefix(region);
    let mut backups: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|error| {
            format!("读取官方客户端登录态目录失败（{}）：{error}", dir.display())
        })?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(prefix) && name.ends_with(".info"))
        })
        .collect();
    // 时间戳在文件名里（ISO 形态、零填充），字典序就是时间序
    backups.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    backups.into_iter().next().ok_or_else(|| {
        format!(
            "未在官方客户端目录找到登录态文件（{}）：请先在官方 WorkBuddy 客户端里登录一次",
            dir.display()
        )
    })
}

// ─── 取值助手 ────────────────────────────────────────────────

/// 嵌套 `auth` 对象优先、顶层兜底，取第一个非空文本。
fn pick_nested(
    auth: Option<&Map<String, Value>>,
    object: &Map<String, Value>,
    keys: &[&str],
) -> String {
    for key in keys {
        if let Some(value) = auth.and_then(|map| map.get(*key)) {
            let text = text_of(value);
            if !text.is_empty() {
                return text;
            }
        }
    }
    for key in keys {
        if let Some(value) = object.get(*key) {
            let text = text_of(value);
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

/// 同上，取第一个正数（毫秒时间戳）。
fn pick_number(
    auth: Option<&Map<String, Value>>,
    object: &Map<String, Value>,
    keys: &[&str],
) -> f64 {
    let read = |value: &Value| -> f64 {
        match value {
            Value::Number(number) => number.as_f64().unwrap_or(0.0),
            Value::String(text) => text.trim().parse::<f64>().unwrap_or(0.0),
            _ => 0.0,
        }
    };
    for key in keys {
        if let Some(value) = auth.and_then(|map| map.get(*key)) {
            let number = read(value);
            if number > 0.0 {
                return number;
            }
        }
    }
    for key in keys {
        if let Some(value) = object.get(*key) {
            let number = read(value);
            if number > 0.0 {
                return number;
            }
        }
    }
    0.0
}

/// JSON 值的文本形态（字符串原样、数字转字符串、其余空串）。
fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.trim().to_string(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    }
}

/// 同上，但接受 `Option`（`None` / `null` 都给空串）。
fn text_of_opt(value: Option<&Value>) -> String {
    value.map(text_of).unwrap_or_default()
}

fn truncate(text: String, max: usize) -> String {
    crate::json::truncate_text(&text, max)
}
