//! 读本机豆包工作客户端的设备号。
//!
//! ── 为什么转发必须带上它（2026-11-03 实测）──────────────────────
//! 同一份 Cookie、同一台机器、同一个出口，只差这一个参数：
//!   · 只带 Cookie（改造前的做法）→ 上游回 `STREAM_ERROR` /
//!     `error_code=710022004` / `shark_admin` 的**人机验证（滑块）**，
//!     任何模型、任何账号都一样；
//!   · 补上 `device_id` 与它派生出的 `fp=verify_<device_id>` → 正常回答；
//!   · 只补版本号与渠道号（`pc_version=2.32.14` / `channel=1001`）→ 照样被拦。
//! 也就是说风控认的是「这个账号在这台受信设备上」，而设备号只在客户端的配置
//! 文件里（`Local State` 的 `aha.device.device_id`）：Cookie 里没有这个键，
//! 协议也不下发 —— 不去读它就永远是"未受信设备"，转发必然被拦。
//!
//! ── 读不到怎么办 ────────────────────────────────────────────
//! 客户端没装、文件不在、格式变了、值不合规，一律当"读不到"，行为与改造前
//! 完全一致（没有设备号可带）。读这个文件绝不能把转发弄挂，所以全程失败即 None。

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

/// `Local State` 是 Chromium 的配置文件，正常只有几十到几百 KB。
/// 给个上限：一个被换过的巨大文件不该被整份读进内存。
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// 设备号长度上限（正常是 15 位左右的数字串）。
const MAX_DEVICE_BYTES: usize = 64;
/// 结果缓存时长：设备号几乎不变，但重装客户端后会变，所以不缓存整个进程期。
const TTL: Duration = Duration::from_secs(60);

fn cache() -> &'static Mutex<Option<(std::time::Instant, Option<String>)>> {
    static CACHE: std::sync::OnceLock<Mutex<Option<(std::time::Instant, Option<String>)>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

/// 客户端的 `Local State` 路径（按平台惯例推；与 EchoBird 的客户端数据目录同源）。
fn local_state() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var_os("LOCALAPPDATA")?;
        return Some(
            PathBuf::from(base)
                .join("DoubaoWork")
                .join("User Data")
                .join("Local State"),
        );
    }
    #[cfg(target_os = "macos")]
    {
        let base = std::env::var_os("HOME")?;
        return Some(
            PathBuf::from(base)
                .join("Library")
                .join("Application Support")
                .join("DoubaoWork")
                .join("Local State"),
        );
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".config")))?;
        Some(base.join("DoubaoWork").join("Local State"))
    }
}

/// 值必须能安全地进 query：只收 ASCII 字母数字与 `-`/`_`，其余当读不到。
fn usable(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= MAX_DEVICE_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    .then(|| value.to_string())
}

fn read() -> Option<String> {
    let path = local_state()?;
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let device = &value["aha"]["device"]["device_id"];
    let text = device
        .as_str()
        .map(str::to_string)
        .or_else(|| device.as_u64().map(|id| id.to_string()))?;
    usable(&text)
}

/// 本机客户端的设备号（读不到就是 `None`，缓存 [`TTL`]）。
pub fn device_id() -> Option<String> {
    if let Ok(cache) = cache().lock() {
        if let Some((at, value)) = cache.as_ref() {
            if at.elapsed() < TTL {
                return value.clone();
            }
        }
    }
    let found = read();
    if let Ok(mut cache) = cache().lock() {
        *cache = Some((std::time::Instant::now(), found.clone()));
    }
    if found.is_some() {
        crate::logging::verbose(
            "[DoubaoWork]",
            "已读到本机客户端的设备号，转发时会带上（上游风控认这个设备）",
        );
    }
    found
}
