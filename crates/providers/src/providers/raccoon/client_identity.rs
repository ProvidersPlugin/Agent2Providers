//! 对话进程的身份与页面业务身份分开；系统信息只读取一次。

use std::sync::OnceLock;

const CLIENT_NAME: &str = "@raccoon/office-desktop";
const CLIENT_VERSION: &str = "v1.1.5";
const SDK_VERSION: &str = "2.8.0";
const PYTHON_VERSION: &str = "3.12.13";

pub(super) fn headers() -> Vec<(String, String)> {
    let mut headers = vec![
        (
            "User-Agent".to_string(),
            format!("AsyncOpenAI/Python {SDK_VERSION}"),
        ),
        ("X-Stainless-Lang".to_string(), "python".to_string()),
        (
            "X-Stainless-Package-Version".to_string(),
            SDK_VERSION.to_string(),
        ),
        ("X-Stainless-OS".to_string(), stainless_os().to_string()),
        ("X-Stainless-Arch".to_string(), stainless_arch().to_string()),
        ("X-Stainless-Runtime".to_string(), "CPython".to_string()),
        (
            "X-Stainless-Runtime-Version".to_string(),
            PYTHON_VERSION.to_string(),
        ),
        ("X-Stainless-Async".to_string(), "async:asyncio".to_string()),
        ("X-Stainless-Retry-Count".to_string(), "0".to_string()),
        (
            "X-Stainless-Read-Timeout".to_string(),
            format!(
                "{}.0",
                crate::timeouts::current().read_timeout_backstop_ms() / 1000
            ),
        ),
        ("X-Client-Name".to_string(), CLIENT_NAME.to_string()),
        (
            "X-Client-Platform".to_string(),
            format!("desktop-{}-{}", desktop_os(), desktop_arch()),
        ),
        ("X-Client-Version".to_string(), CLIENT_VERSION.to_string()),
        ("X-Client-Channel".to_string(), "official".to_string()),
    ];
    static OS_VERSION: OnceLock<String> = OnceLock::new();
    let version = OS_VERSION.get_or_init(os_version);
    if let Some(version) = clean(version) {
        headers.push(("X-Client-OS-Version".to_string(), version));
    }
    headers
}

fn clean(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
        return None;
    }
    Some(value.chars().take(256).collect())
}

fn desktop_os() -> &'static str {
    match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "macos",
        "linux" => "linux",
        _ => "unknown",
    }
}

fn desktop_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "x86" => "x86",
        "aarch64" => "arm64",
        "arm" => "arm",
        _ => "unknown",
    }
}

fn stainless_os() -> &'static str {
    match std::env::consts::OS {
        "windows" => "Windows",
        "macos" => "MacOS",
        "linux" => "Linux",
        "freebsd" => "FreeBSD",
        "openbsd" => "OpenBSD",
        _ => "Unknown",
    }
}

fn stainless_arch() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        // Python 的 platform.machine() 在 Windows x64 上返回 AMD64。
        ("windows", "x86_64") => "other:amd64",
        (_, "x86_64") => "x64",
        (_, "x86") => "x32",
        (_, "aarch64") => "arm64",
        (_, "arm") => "arm",
        _ => "unknown",
    }
}

#[cfg(windows)]
fn os_version() -> String {
    use windows::core::w;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

    let mut buffer = [0u16; 64];
    let mut size = std::mem::size_of_val(&buffer) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
            w!("CurrentBuildNumber"),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if status.is_err() {
        return String::new();
    }
    let length = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    let build = String::from_utf16_lossy(&buffer[..length]);
    match build.trim().parse::<u32>() {
        Ok(build) => format!("10.0.{build}"),
        Err(_) => String::new(),
    }
}

#[cfg(target_os = "macos")]
fn os_version() -> String {
    let Ok(text) = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist")
    else {
        return String::new();
    };
    text.split_once("<key>ProductVersion</key>")
        .and_then(|(_, rest)| rest.split_once("<string>"))
        .and_then(|(_, rest)| rest.split_once("</string>"))
        .map(|(value, _)| value.trim().to_string())
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
fn os_version() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|value| value.trim().to_string())
        .unwrap_or_default()
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn os_version() -> String {
    String::new()
}
