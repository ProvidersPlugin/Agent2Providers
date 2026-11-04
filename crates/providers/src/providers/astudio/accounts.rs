//! AStudio 账号：读桌面端登录态、粘贴 modelBearerToken、公开视图与去重。
//!
//! ── 桌面端把登录态放在哪 ────────────────────────────────────
//! ```text
//!   <DataRoot>/userdata/astron-session.json
//!       {modelBearerToken, token, accountId, uid, ssoSessionId, banned?}
//!   <DataRoot>/runtime/acode-home/config.toml
//!       [model_providers.astron-spark] experimental_bearer_token = "…"
//! ```
//! DataRoot 的找法（按顺序）：
//!   1. `ASTUDIO_HOME` / `SYNARA_HOME` 环境变量；
//!   2. Windows 注册表 `HKCU\Software\AStudioStorage` 的
//!      `Production/Canary/DevelopmentDataRoot`；
//!   3. `~/.acode/acode/`。
//!
//! ── 两套凭据，各管一段 ──────────────────────────────────────
//! `modelBearerToken` 只够调模型；积分 / 会员 / 签到走 `agent.xfyun.cn` 的
//! Cookie 会话（`token` + `accountId` + 可选 `ssoSessionId`）。所以只有模型
//! 凭据的账号**能用对话**，但没有余额与签到 —— 这是上游事实，不是省事。

use std::path::{Path, PathBuf};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::endpoints;
use crate::limits;

/// 当前毫秒时间戳。
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// 一份 AStudio 凭据。
#[derive(Clone, Default)]
pub struct Credentials {
    pub name: String,
    /// modelBearerToken（调模型）
    pub key: String,
    pub uid: String,
    /// 网页会话 token（积分 / 签到）
    pub token: String,
    pub account_id: String,
    pub sso_session_id: String,
    /// `desktop` / `paste`
    pub source: String,
    /// 读桌面端时命中的文件（排错用，不显示在界面上）
    pub origin: String,
}

fn text(secret: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = secret.get(*key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    String::new()
}

impl Credentials {
    pub fn from_secret(secret: &Value) -> Self {
        let session = secret.get("session").cloned().unwrap_or(Value::Null);
        Self {
            name: text(secret, &["name"]),
            key: text(secret, &["key", "modelBearerToken", "apiKey"]),
            uid: text(secret, &["uid"]),
            token: text(&session, &["token"]),
            account_id: text(&session, &["accountId"]),
            sso_session_id: text(&session, &["ssoSessionId"]),
            source: text(secret, &["source"]),
            origin: text(secret, &["origin"]),
        }
    }

    pub fn write_into(&self, secret: &mut Value) {
        let Some(object) = secret.as_object_mut() else {
            *secret = json!({});
            return;
        };
        object.insert("key".into(), json!(self.key));
        object.insert("uid".into(), json!(self.uid));
        object.insert("source".into(), json!(self.source));
        if !self.name.is_empty() {
            object.insert("name".into(), json!(self.name));
        }
        if !self.origin.is_empty() {
            object.insert("origin".into(), json!(self.origin));
        }
        if !self.token.is_empty() && !self.account_id.is_empty() {
            let mut session = json!({
                "token": self.token,
                "accountId": self.account_id,
            });
            if !self.sso_session_id.is_empty() {
                session["ssoSessionId"] = json!(self.sso_session_id);
            }
            object.insert("session".into(), session);
        }
    }

    /// 有网页会话（余额与签到要它）。
    pub fn has_session(&self) -> bool {
        !self.token.is_empty() && !self.account_id.is_empty()
    }

    /// 账号面 Cookie 头（与桌面端 `buildAstronCookieHeader` 同序）。
    pub fn cookie_header(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.sso_session_id.is_empty() {
            parts.push(format!("ssoSessionId={}", self.sso_session_id));
            parts.push(format!("sso_sessionid={}", self.sso_session_id));
        }
        if !self.account_id.is_empty() {
            parts.push(format!("account_id={}", self.account_id));
        }
        if !self.token.is_empty() {
            parts.push(format!("token={}", self.token));
        }
        parts.join("; ")
    }
}

/// 桌面端的数据根候选（异步：注册表查询要走子进程）。
pub async fn desktop_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for variable in ["ASTUDIO_HOME", "SYNARA_HOME"] {
        if let Ok(value) = std::env::var(variable) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                roots.push(PathBuf::from(trimmed));
            }
        }
    }
    if cfg!(target_os = "windows") {
        for prefix in endpoints::REGISTRY_PREFIXES {
            if let Some(value) = read_registry(&format!("{prefix}DataRoot")).await {
                roots.push(PathBuf::from(value));
            }
        }
    }
    roots
}

/// 读一个注册表值。
///
/// ── 为什么不用 `reg query` 子进程 ──────────────────────────
/// `reg.exe` 的 stdout 是控制台的 OEM 代码页：中文 Windows 上数据目录若带
/// 中文用户名，用 UTF-8 解会直接乱码，表现为「装了客户端但读不到登录态」。
/// 这里直接调 `RegGetValueW`，拿到的是 UTF-16，与系统语言无关。
async fn read_registry(value_name: &str) -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        let name = value_name.to_string();
        return tokio::task::spawn_blocking(move || registry_value(&name))
            .await
            .ok()
            .flatten();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = value_name;
        None
    }
}

#[cfg(target_os = "windows")]
fn registry_value(value_name: &str) -> Option<String> {
    use windows::core::HSTRING;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    let subkey = HSTRING::from(
        endpoints::REGISTRY_KEY
            .strip_prefix("HKCU\\")
            .unwrap_or(endpoints::REGISTRY_KEY),
    );
    let value = HSTRING::from(value_name);
    let mut size: u32 = 0;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &subkey,
            &value,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if !status.is_ok() || size == 0 {
        return None;
    }
    let mut buffer = vec![0u16; (size as usize / 2).saturating_add(1)];
    let mut size = (buffer.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &subkey,
            &value,
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr() as *mut core::ffi::c_void),
            Some(&mut size),
        )
    };
    if !status.is_ok() {
        return None;
    }
    let length = (size as usize / 2).saturating_sub(1).min(buffer.len());
    let text = String::from_utf16_lossy(&buffer[..length]);
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// 读桌面端登录态。
pub async fn read_desktop() -> Result<Credentials, String> {
    let mut session_paths: Vec<PathBuf> = Vec::new();
    let mut config_paths: Vec<PathBuf> = Vec::new();
    for root in desktop_roots().await {
        session_paths.push(root.join("userdata").join("astron-session.json"));
        config_paths.push(root.join("runtime").join("acode-home").join("config.toml"));
    }
    let fallback = home().join(".acode").join("acode");
    session_paths.push(fallback.join("userdata").join("astron-session.json"));
    config_paths.push(fallback.join("config.toml"));

    let mut problems: Vec<String> = Vec::new();
    for path in &session_paths {
        let Ok(text) = read_candidate(path, &mut problems) else {
            continue;
        };
        let value: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(_) => {
                problems.push(format!("{} 不是有效 JSON", path.display()));
                continue;
            }
        };
        if value.get("banned").and_then(Value::as_bool) == Some(true) {
            return Err(format!("AStudio 账号已被封禁（{}）", path.display()));
        }
        let key = text_of(&value, "modelBearerToken")
            .or_else(|| text_of(&value, "token"))
            .unwrap_or_default();
        if key.is_empty() {
            problems.push(format!("{} 里没有 modelBearerToken/token", path.display()));
            continue;
        }
        if key.len() > 4096 {
            problems.push(format!("{} 的凭据异常地长", path.display()));
            continue;
        }
        return Ok(Credentials {
            key,
            uid: text_of(&value, "uid").unwrap_or_default(),
            token: text_of(&value, "token").unwrap_or_default(),
            account_id: text_of(&value, "accountId").unwrap_or_default(),
            sso_session_id: text_of(&value, "ssoSessionId").unwrap_or_default(),
            source: "desktop".into(),
            origin: path.display().to_string(),
            ..Default::default()
        });
    }
    for path in &config_paths {
        let Ok(text) = read_candidate(path, &mut problems) else {
            continue;
        };
        if let Some((key, uid)) = config_token(&text) {
            return Ok(Credentials {
                key,
                uid,
                source: "desktop".into(),
                origin: path.display().to_string(),
                ..Default::default()
            });
        }
        problems.push(format!("{} 里没有 astron-spark 凭据", path.display()));
    }
    let detail = if problems.is_empty() {
        String::new()
    } else {
        format!("：{}", problems.join("；"))
    };
    Err(if problems.is_empty() {
        "未找到 AStudio 桌面端登录态。请先启动 AStudio 并登录，或改用「粘贴 modelBearerToken」方式。"
            .to_string()
    } else {
        format!(
            "AStudio 桌面端登录态读不出来{detail}。请先启动 AStudio 并登录，或改用「粘贴 modelBearerToken」方式。"
        )
    })
}

fn text_of(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn read_candidate(path: &Path, problems: &mut Vec<String>) -> Result<String, ()> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(()),
        Err(error) => {
            problems.push(format!("{} 读取失败（{error}）", path.display()));
            Err(())
        }
    }
}

/// 从 acode 的 `config.toml` 里取 `[model_providers.astron-spark]` 的凭据。
fn config_token(content: &str) -> Option<(String, String)> {
    let mut inside = false;
    let mut token = String::new();
    let mut uid = String::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            inside = trimmed
                .strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(']'))
                .is_some_and(|name| name.trim() == "model_providers.astron-spark");
            continue;
        }
        if !inside {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            let value = value.trim().trim_matches('"');
            match key.trim() {
                "experimental_bearer_token" => token = value.to_string(),
                "uid" => uid = value.to_string(),
                _ => {}
            }
        }
    }
    if token.is_empty() {
        None
    } else {
        Some((token, uid))
    }
}

/// 归一化一条账号。
pub async fn normalize(
    _ctx: &PluginCtx,
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, String> {
    let name = input.name.clone().unwrap_or_default();
    let credentials = match input.source {
        proto::NormalizeSource::Desktop => read_desktop().await?,
        proto::NormalizeSource::Manual { payload } => {
            // 动态登录方式（methodIndex + inputs）把字段包在 inputs 里；
            // 旧入口直接把字段放在顶层。两种都认。
            let fields = payload
                .get("inputs")
                .filter(|value| value.is_object())
                .unwrap_or(&payload);
            from_payload(fields)?
        }
        proto::NormalizeSource::FileText { content, .. } => {
            let text = content.trim();
            if text.starts_with('{') {
                from_json(text)?
            } else if text.is_empty() {
                return Err("文件是空的".to_string());
            } else {
                paste_credential(text, "", "", "", "", "")?
            }
        }
        proto::NormalizeSource::FileBase64 { .. } => {
            return Err(
                "AStudio 不支持导入二进制文件，请粘贴 modelBearerToken 或导入本机登录态".into(),
            )
        }
    };
    Ok(account_of(credentials, &name))
}

fn from_payload(payload: &Value) -> Result<Credentials, String> {
    let key = text(payload, &["modelBearerToken", "key", "apiKey", "token"]);
    if key.is_empty() {
        return Err("请填写 modelBearerToken".to_string());
    }
    paste_credential(
        &key,
        &text(payload, &["uid"]),
        &text(payload, &["sessionToken"]),
        &text(payload, &["accountId"]),
        &text(payload, &["ssoSessionId"]),
        "",
    )
}

fn from_json(text: &str) -> Result<Credentials, String> {
    let value: Value = serde_json::from_str(text).map_err(|_| {
        "这不是有效的 JSON，请粘贴 astron-session.json 或 modelBearerToken".to_string()
    })?;
    if value.get("banned").and_then(Value::as_bool) == Some(true) {
        return Err("AStudio 账号已被封禁".to_string());
    }
    let key = text_of(&value, "modelBearerToken")
        .or_else(|| text_of(&value, "token"))
        .ok_or_else(|| "JSON 里没有 modelBearerToken/token".to_string())?;
    paste_credential(
        &key,
        &text_of(&value, "uid").unwrap_or_default(),
        &text_of(&value, "token").unwrap_or_default(),
        &text_of(&value, "accountId").unwrap_or_default(),
        &text_of(&value, "ssoSessionId").unwrap_or_default(),
        "",
    )
}

fn paste_credential(
    key: &str,
    uid: &str,
    token: &str,
    account_id: &str,
    sso_session_id: &str,
    name: &str,
) -> Result<Credentials, String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("modelBearerToken 必填".to_string());
    }
    if key.len() > limits::MAX_TOKEN_LENGTH {
        return Err("这份凭据异常地长，请确认只粘贴了 modelBearerToken".to_string());
    }
    Ok(Credentials {
        name: name.to_string(),
        key: key.to_string(),
        uid: uid.trim().to_string(),
        token: token.trim().to_string(),
        account_id: account_id.trim().to_string(),
        sso_session_id: sso_session_id.trim().to_string(),
        source: "paste".into(),
        origin: String::new(),
    })
}

/// 从一份凭据构造 secret（登录流程与手动添加共用）。
pub fn secret_of(credentials: &Credentials) -> Value {
    let mut secret = json!({});
    credentials.write_into(&mut secret);
    secret
}

fn account_of(credentials: Credentials, remark: &str) -> proto::NormalizedAccount {
    let mut secret = json!({});
    let mut credentials = credentials;
    if !remark.is_empty() {
        credentials.name = remark.to_string();
    }
    credentials.write_into(&mut secret);
    let identity = identity_of(&credentials);
    let view = public(&secret);
    proto::NormalizedAccount {
        secret,
        public: view,
        identity,
        desktop: credentials.source == "desktop",
    }
}

/// 公开视图。
pub fn public(secret: &Value) -> Value {
    let credentials = Credentials::from_secret(secret);
    let name = if !credentials.name.is_empty() {
        credentials.name.clone()
    } else if !credentials.uid.is_empty() {
        credentials.uid.clone()
    } else if !credentials.account_id.is_empty() {
        credentials.account_id.clone()
    } else {
        "AStudio".to_string()
    };
    let mut view = json!({
        "name": name,
        "uid": credentials.uid.clone(),
        "accountId": credentials.account_id.clone(),
        "tokenTail": tail_of(&credentials.key),
        "hasCredentials": !credentials.key.is_empty(),
    });
    if credentials.source == "desktop" {
        view["desktop"] = json!(true);
    }
    if !credentials.has_session() {
        view["noSession"] = json!(true);
    }
    view
}

/// 去重身份：uid 优先，其次 key 的哈希。
pub fn identity_of(credentials: &Credentials) -> String {
    if !credentials.uid.is_empty() {
        return format!("astudio:{}", credentials.uid);
    }
    if !credentials.account_id.is_empty() {
        return format!("astudio:{}", credentials.account_id);
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(credentials.key.as_bytes());
    let hex: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("astudio:key:{hex}")
}

pub fn identity(secret: &Value) -> String {
    identity_of(&Credentials::from_secret(secret))
}

fn tail_of(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let start = chars.len().saturating_sub(4);
    chars[start..].iter().collect()
}
