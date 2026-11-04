//! Kiro 本机登录态：读 kiro-cli 的 SQLite 与 Kiro IDE 的 token 文件。
//!
//! ── 两种来源 ────────────────────────────────────────────────
//! ```text
//!   kiro-cli   %APPDATA%\kiro-cli\data.sqlite3（macOS / Linux 见 cli_path）
//!              auth_kv 表：kirocli:social:token / kirocli:odic:token /
//!              kirocli:external-idp:token，value 是 JSON
//!   Kiro IDE   ~/.aws/sso/cache/kiro-auth-token.json
//!              clientIdHash 指向同目录的 <hash>.json（AWS OIDC 客户端）
//! ```
//!
//! ── 为什么写回 ──────────────────────────────────────────────
//! 一次刷新可能轮换 refresh_token。只读不写回，会让 kiro-cli / IDE 手里的
//! 那份变成旧的，下次它们自己刷新时用旧 refresh_token 换不出来。参考实现
//! 的做法是刷新后原地写回（`saveBack`），这里照做 —— 写失败只记日志，
//! 不让转发失败。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 本 crate 编译期禁止 unwrap/expect/panic。SQLite 是同步 API，调用方必须
//! 放进 `spawn_blocking`（本文件的函数全部是同步的）。

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::credentials::{now_ms, Credentials};
use crate::{logging, timeutil};

/// home 目录（Windows 用 USERPROFILE，其余 HOME）。
fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// kiro-cli 的 SQLite 路径。
pub fn cli_path() -> PathBuf {
    if cfg!(target_os = "windows") {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join("AppData").join("Roaming"));
        return base.join("kiro-cli").join("data.sqlite3");
    }
    if cfg!(target_os = "macos") {
        return home()
            .join("Library")
            .join("Application Support")
            .join("kiro-cli")
            .join("data.sqlite3");
    }
    home().join(".local/share/kiro-cli/data.sqlite3")
}

/// Kiro IDE 的 token 文件路径。
pub fn ide_path() -> PathBuf {
    home()
        .join(".aws")
        .join("sso")
        .join("cache")
        .join("kiro-auth-token.json")
}

/// 从 kiro-cli 的 `auth_kv` 读一条 JSON。
fn read_kv(connection: &rusqlite::Connection, key: &str) -> Option<Value> {
    let mut statement = connection
        .prepare("SELECT value FROM auth_kv WHERE key = ?1")
        .ok()?;
    let text: String = statement.query_row([key], |row| row.get(0)).ok()?;
    serde_json::from_str(&text).ok()
}

/// 读 kiro-cli 的登录态。
pub fn read_cli() -> Option<Credentials> {
    let path = cli_path();
    if !path.is_file() {
        return None;
    }
    let connection =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    for kind in ["social", "odic", "external-idp"] {
        let key = format!("kirocli:{kind}:token");
        let Some(token) = read_kv(&connection, &key) else {
            continue;
        };
        let access = token
            .get("access_token")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if access.is_empty() {
            continue;
        }
        let mut credentials = Credentials {
            method: if kind == "social" {
                "social".into()
            } else if kind == "odic" {
                "idc".into()
            } else {
                "external-idp".into()
            },
            access,
            refresh: token
                .get("refresh_token")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            expires: token
                .get("expires_at")
                .and_then(Value::as_str)
                .and_then(timeutil::parse_rfc3339_ms)
                .unwrap_or(0),
            region: token
                .get("region")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or("us-east-1")
                .to_string(),
            profile: token
                .get("profile_arn")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            db_key: key.clone(),
            source: "kiro-cli".into(),
            ..Default::default()
        };
        if kind == "odic" {
            let start = token.get("start_url").and_then(Value::as_str).unwrap_or("");
            credentials.login_provider =
                if start.is_empty() || start == super::endpoints::BUILDER_ID_START {
                    "builderid".into()
                } else {
                    "enterprise".into()
                };
            credentials.uid = if credentials.login_provider == "builderid" {
                "builderid".into()
            } else {
                "enterprise".into()
            };
            if let Some(registration) = read_kv(&connection, "kirocli:odic:device-registration") {
                credentials.client_id = registration
                    .get("client_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                credentials.client_secret = registration
                    .get("client_secret")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
            }
        } else if kind == "external-idp" {
            credentials.client_id = token
                .get("client_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            credentials.token_url = token
                .get("token_endpoint")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    token
                        .get("issuer_url")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .map(|value| format!("{}/v1/token", value.trim_end_matches('/')))
                })
                .unwrap_or_default();
            credentials.login_provider = "external-idp".into();
        }
        return Some(credentials);
    }
    None
}

/// 读 Kiro IDE 的登录态。
pub fn read_ide() -> Option<Credentials> {
    let path = ide_path();
    let text = std::fs::read_to_string(&path).ok()?;
    let token: Value = serde_json::from_str(&text).ok()?;
    let access = token
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if access.is_empty() {
        return None;
    }
    let mut credentials = Credentials {
        access,
        refresh: token
            .get("refreshToken")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        expires: token
            .get("expiresAt")
            .and_then(Value::as_str)
            .and_then(timeutil::parse_rfc3339_ms)
            .unwrap_or(0),
        region: token
            .get("region")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or("us-east-1")
            .to_string(),
        profile: token
            .get("profileArn")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        method: "idc".into(),
        source: "kiro-ide".into(),
        ide_path: path.to_string_lossy().to_string(),
        ..Default::default()
    };
    let builder = token
        .get("provider")
        .and_then(Value::as_str)
        .is_some_and(|value| value.eq_ignore_ascii_case("builderid"));
    credentials.login_provider = if builder {
        "builderid".into()
    } else {
        "enterprise".into()
    };
    credentials.uid = credentials.login_provider.clone();
    if let Some(hash) = token.get("clientIdHash").and_then(Value::as_str) {
        let registration_path = path
            .parent()
            .map(|dir| dir.join(format!("{hash}.json")))
            .unwrap_or_else(|| PathBuf::from(format!("{hash}.json")));
        if let Ok(registration_text) = std::fs::read_to_string(&registration_path) {
            if let Ok(registration) = serde_json::from_str::<Value>(&registration_text) {
                credentials.client_id = registration
                    .get("clientId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                credentials.client_secret = registration
                    .get("clientSecret")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
            }
        }
    }
    let social = token
        .get("authMethod")
        .and_then(Value::as_str)
        .is_some_and(|value| value.eq_ignore_ascii_case("social"));
    if social || credentials.client_id.is_empty() {
        credentials.method = "social".into();
    }
    Some(credentials)
}

/// 按账号记录的来源读一次实时登录态。
pub fn read_source(source: &str) -> Option<Credentials> {
    match source {
        "kiro-cli" => read_cli(),
        "kiro-ide" => read_ide(),
        "kiro" => read_cli().or_else(read_ide),
        _ => None,
    }
}

/// 把刷新后的凭证写回它原来的位置（失败只记日志）。
pub fn save_back(credentials: &Credentials) {
    if credentials.db_key.is_empty() && credentials.ide_path.is_empty() {
        return;
    }
    if !credentials.db_key.is_empty() {
        if let Err(error) = save_cli(credentials) {
            logging::warn("kiro", &format!("写回 kiro-cli 登录态失败：{error}"));
        }
        return;
    }
    if let Err(error) = save_ide(credentials) {
        logging::warn("kiro", &format!("写回 Kiro IDE 登录态失败：{error}"));
    }
}

fn save_cli(credentials: &Credentials) -> Result<(), String> {
    let path = cli_path();
    let connection = rusqlite::Connection::open(&path).map_err(|error| error.to_string())?;
    let _ = connection.busy_timeout(std::time::Duration::from_millis(3000));
    let mut statement = connection
        .prepare("SELECT value FROM auth_kv WHERE key = ?1")
        .map_err(|error| error.to_string())?;
    let text: String = statement
        .query_row([&credentials.db_key], |row| row.get(0))
        .map_err(|error| error.to_string())?;
    let mut value: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "access_token".into(),
            Value::String(credentials.access.clone()),
        );
        object.insert(
            "refresh_token".into(),
            Value::String(credentials.refresh.clone()),
        );
        object.insert(
            "expires_at".into(),
            Value::String(iso8601(credentials.expires)),
        );
        if !credentials.profile.is_empty() && credentials.method == "social" {
            object.insert(
                "profile_arn".into(),
                Value::String(credentials.profile.clone()),
            );
        }
    }
    drop(statement);
    let encoded = serde_json::to_string(&value).map_err(|error| error.to_string())?;
    connection
        .execute(
            "UPDATE auth_kv SET value = ?1 WHERE key = ?2",
            rusqlite::params![encoded, credentials.db_key],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn save_ide(credentials: &Credentials) -> Result<(), String> {
    let path = Path::new(&credentials.ide_path);
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let mut value: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "accessToken".into(),
            Value::String(credentials.access.clone()),
        );
        object.insert(
            "refreshToken".into(),
            Value::String(credentials.refresh.clone()),
        );
        object.insert(
            "expiresAt".into(),
            Value::String(iso8601(credentials.expires)),
        );
    }
    let encoded = serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?;
    let temporary = format!("{}.agent2api-tmp", credentials.ide_path);
    std::fs::write(&temporary, encoded).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, path).map_err(|error| error.to_string())?;
    Ok(())
}

/// 毫秒时间戳 → `YYYY-MM-DDTHH:MM:SSZ`（写回 SQLite / JSON 用）。
pub fn iso8601(ms: i64) -> String {
    if ms <= 0 {
        return iso8601(now_ms());
    }
    let seconds = ms / 1000;
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// Howard Hinnant 的 `civil_from_days`（epoch 天数 → 年月日）。
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

/// 找 kiro-cli 可执行文件（PATH 与几个常见安装位置）。
pub fn kiro_executable() -> Option<PathBuf> {
    let name = if cfg!(target_os = "windows") {
        "kiro-cli.exe"
    } else {
        "kiro-cli"
    };
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let candidates = [
        home().join(".local/bin/kiro-cli"),
        PathBuf::from("/Applications/Kiro CLI.app/Contents/MacOS/kiro-cli"),
        PathBuf::from("/usr/local/bin/kiro-cli"),
        PathBuf::from("/opt/homebrew/bin/kiro-cli"),
    ];
    candidates.into_iter().find(|path| path.is_file())
}

/// 让 kiro-cli 自己续期一次（它比我们更清楚自己的凭证形态）。
pub async fn refresh_with_cli() -> Option<Credentials> {
    let executable = kiro_executable()?;
    let mut command = tokio::process::Command::new(executable);
    command.args(["debug", "refresh-auth-token"]);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(20), command.status()).await;
    tokio::task::spawn_blocking(read_cli).await.ok().flatten()
}

/// 当前主机名（IDE UA 用）。
pub fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}
