//! 直接读豆包工作客户端本地的 Cookie 数据库（**不需要调试端口**）。
//!
//! ── 为什么要多这一条路（2026-11-03）─────────────────────────
//! 之前的「导入本机登录态」只能通过客户端调试端口（CDP，`--remote-debugging-port=9342`）
//! 读 Cookie。用户抱怨得很对：导入一次登录态而已，凭什么要我先带参数重启客户端？
//! 客户端本身就是 Chromium，Cookie 就存在本地 SQLite 里 —— 直接读它就行。
//!
//! ── 加密怎么解（Windows）───────────────────────────────────
//! Chromium 的 `Cookies` 表里 `encrypted_value` 是 os_crypt 密文：
//! `v10` + 12 字节 nonce + 密文 + 16 字节 tag（AES-256-GCM）。密钥在
//! `Local State` 的 `os_crypt.encrypted_key`，外面裹了一层 DPAPI。
//! 这两步本仓库已有现成实现（`crate::crypto` 的 `os_crypt_aes_key` /
//! `decrypt_os_crypt_blob`，Autoclaw 的 token 解密用的同一套），这里直接复用。
//!
//! 明文布局：`SHA256(host_key)` 32 字节 + Cookie 值（Chromium ≥ v24 的惯例，
//! 本机实测一致）。按 host_key 校验后再剥掉，对不上就按无前缀处理 ——
//! 宁可多带 32 字节也不要把正常值截掉。
//!
//! ── 为什么先复制一份再读 ────────────────────────────────────
//! 客户端运行时会持有 SQLite 的锁并持续写入。直接开原库可能被锁住，读到一半
//! 也可能撞上写入。先把库连同 `-journal` / `-wal` / `-shm` 复制到临时目录再
//! 只读打开，既避开锁，也不会影响客户端。**只读，绝不写回**。
//!
//! ── 非 Windows ──────────────────────────────────────────────
//! DPAPI 只有 Windows 有，macOS 走 Keychain（未实现）。这些平台上本模块
//! 直接返回错误，由调用方回落到调试端口那条路径（行为与改造前一致）。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::errors::GatewayError;

/// Cookie 库 / 临时副本的大小上限（正常几百 KB；给足余量，防换过的怪文件）。
const MAX_DB_BYTES: u64 = 32 * 1024 * 1024;
/// 单条 Cookie 值上限（`LARK_SUITE_ACCESS_TOKEN` 这类实测 1.3 KB）。
const MAX_COOKIE_BYTES: usize = 16 * 1024;
/// 返回的 Cookie 条数上限（实测豆包域 30 余条）。
const MAX_COOKIES: usize = 256;

/// 客户端 `User Data` 目录（Chromium profile 的父目录）。
fn user_data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var_os("LOCALAPPDATA")?;
        return Some(PathBuf::from(base).join("DoubaoWork").join("User Data"));
    }
    #[cfg(target_os = "macos")]
    {
        let base = std::env::var_os("HOME")?;
        return Some(
            PathBuf::from(base)
                .join("Library")
                .join("Application Support")
                .join("DoubaoWork"),
        );
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".config")))?;
        Some(base.join("DoubaoWork"))
    }
}

/// `Local State` 路径（os_crypt 密钥的来源；与 `super::client` 同一份文件）。
fn local_state_path(user_data: &Path) -> PathBuf {
    user_data.join("Local State")
}

/// 所有可能装了登录态的 profile 目录（`Default` / `Profile *`），新的排在后面。
fn profiles(user_data: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(user_data) else {
        return found;
    };
    let mut default = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "Default" {
            default = Some(entry.path());
            continue;
        }
        if name.starts_with("Profile ") && entry.path().join("Network").join("Cookies").is_file() {
            found.push(entry.path());
        }
    }
    if let Some(default) = default {
        if default.join("Network").join("Cookies").is_file() {
            found.insert(0, default);
        }
    }
    found.sort();
    found
}

/// 复制一份库文件（连同日志文件）到临时目录，返回副本路径。
///
/// SQLite 在复制过程中仍可能被客户端写入，但 `-journal` 一并复制后打开时
/// 会自动回滚到一致状态；读失败只会让本次导入回落到调试端口方案。
fn copy_db(source: &Path) -> Result<PathBuf, String> {
    let meta =
        std::fs::metadata(source).map_err(|error| format!("无法读取客户端 Cookie 库：{error}"))?;
    if !meta.is_file() || meta.len() > MAX_DB_BYTES {
        return Err("客户端 Cookie 库大小异常".to_string());
    }
    let dir = std::env::temp_dir().join(crate::ids::random_id("doubaowork_cookies"));
    std::fs::create_dir_all(&dir).map_err(|error| format!("无法创建临时目录：{error}"))?;
    let target = dir.join("Cookies");
    std::fs::copy(source, &target).map_err(|error| {
        let _ = std::fs::remove_dir_all(&dir);
        format!("复制客户端 Cookie 库失败：{error}")
    })?;
    for suffix in ["-journal", "-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{suffix}", source.display()));
        if from.is_file() {
            let to = PathBuf::from(format!("{}{suffix}", target.display()));
            let _ = std::fs::copy(from, to);
        }
    }
    Ok(target)
}

/// 解一条 `encrypted_value`：v10 密文 → 去掉 `SHA256(host_key)` 前缀 → UTF-8。
fn decrypt_value(host: &str, blob: &[u8], key: &[u8]) -> Option<String> {
    if blob.len() > MAX_COOKIE_BYTES {
        return None;
    }
    let plaintext = crate::crypto::decrypt_os_crypt_blob(blob, key).ok()?;
    let expected = Sha256::digest(host.as_bytes());
    let value = if plaintext.len() > 32 && plaintext[..32] == expected[..] {
        &plaintext[32..]
    } else {
        &plaintext[..]
    };
    String::from_utf8(value.to_vec()).ok()
}

/// Chromium 时间（1601-01-01 起的微秒）→ Unix 秒；`0` 表示会话 Cookie（不带过期）。
fn unix_seconds(expires_utc: i64) -> Option<i64> {
    const EPOCH_DELTA_SECS: i64 = 11_644_473_600;
    if expires_utc <= 0 {
        return None;
    }
    Some(expires_utc / 1_000_000 - EPOCH_DELTA_SECS)
}

fn read_profile(profile: &Path, key: &[u8]) -> Result<Vec<Value>, String> {
    let db = profile.join("Network").join("Cookies");
    if !db.is_file() {
        return Ok(Vec::new());
    }
    let copy = copy_db(&db)?;
    let result = read_copy(&copy, key);
    if let Some(dir) = copy.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
    result
}

fn read_copy(path: &Path, key: &[u8]) -> Result<Vec<Value>, String> {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("打开客户端 Cookie 库失败：{error}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT host_key, name, value, encrypted_value, path, expires_utc \
             FROM cookies WHERE host_key LIKE '%doubao.com%'",
        )
        .map_err(|error| format!("读取客户端 Cookie 库失败：{error}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .map_err(|error| format!("读取客户端 Cookie 库失败：{error}"))?;
    let mut cookies = Vec::new();
    for row in rows.flatten() {
        if cookies.len() >= MAX_COOKIES {
            break;
        }
        let (host, name, plain, encrypted, path, expires) = row;
        let value = if plain.is_empty() {
            match decrypt_value(&host, &encrypted, key) {
                Some(value) if !value.is_empty() => value,
                _ => continue,
            }
        } else {
            plain
        };
        if value.len() > MAX_COOKIE_BYTES {
            continue;
        }
        cookies.push(json!({
            "domain": host,
            "path": path,
            "name": name,
            "value": value,
            "expires": unix_seconds(expires).map(|secs| secs as f64).unwrap_or(0.0),
        }));
    }
    Ok(cookies)
}

/// 从本机客户端的 Cookie 库读登录态，返回**与调试端口同形**的 `{"cookies": […]}`
/// （`accounts::credentials` 只管形状，不关心来源）。
///
/// 多个 profile 时取第一个含 `sessionid` 的 —— 与「用户实际登录的那个窗口」一致。
pub fn read() -> Result<Value, GatewayError> {
    let user_data = user_data_dir().ok_or_else(|| {
        GatewayError::with_status(400, "未找到豆包工作客户端的数据目录，请先安装并登录客户端")
    })?;
    let key = crate::crypto::os_crypt_aes_key(&local_state_path(&user_data)).map_err(|error| {
        GatewayError::with_status(
            400,
            format!("无法解密客户端 Cookie（{error}）；可改用网页登录或粘贴 Cookie"),
        )
    })?;
    let profiles = profiles(&user_data);
    if profiles.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "豆包工作客户端没有可用的登录数据，请先登录客户端；也可使用网页登录或粘贴 Cookie",
        ));
    }
    let mut first: Option<Vec<Value>> = None;
    for profile in profiles {
        let cookies = match read_profile(&profile, &key) {
            Ok(cookies) => cookies,
            Err(error) => {
                crate::logging::verbose(
                    "[DoubaoWork]",
                    &format!("读取 {} 失败：{error}", profile.display()),
                );
                continue;
            }
        };
        let has_session = cookies.iter().any(|cookie| {
            matches!(cookie["name"].as_str(), Some("sessionid" | "sessionid_ss"))
                && cookie["value"]
                    .as_str()
                    .is_some_and(|value| !value.is_empty())
        });
        if has_session {
            return Ok(json!({ "cookies": cookies }));
        }
        if first.is_none() && !cookies.is_empty() {
            first = Some(cookies);
        }
    }
    match first {
        Some(cookies) => Ok(json!({ "cookies": cookies })),
        None => Err(GatewayError::with_status(
            400,
            "豆包工作客户端里没有找到有效登录态，请先在客户端登录；也可使用网页登录或粘贴 Cookie",
        )),
    }
}
