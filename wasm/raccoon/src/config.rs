//! 客户端声明沿用1.1.5常量；OS族来自受限runtime.platform，不探测环境/注册表/文件。
//! 原外部头文件改为宿主保存的窄设置，只可改客户端身份，不允许覆盖账号认证或组织。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub fn schema() -> Value {
    json!({ "version": 1, "title": "小浣熊客户端兼容设置", "runtime_overrides": false,
        "description": "平台和默认架构由主程序提供。可填写真实客户端版本，必要时覆盖架构；不读取本机外部头文件，也不允许覆盖认证信息。",
        "fields": [
            { "key": "clientVersion", "label": "桌面客户端版本", "type": "string", "default": "v1.1.5",
                "trim": true, "empty_is_default": true, "max_length": 32, "pattern": "^v[0-9][0-9A-Za-z._-]*$" },
            { "key": "clientArch", "label": "客户端架构", "type": "select", "default": "auto", "options": [
                { "value": "auto", "label": "自动" }, { "value": "unknown", "label": "未提供" }, { "value": "x64", "label": "x64" },
                { "value": "arm64", "label": "ARM64" }, { "value": "x86", "label": "x86" }, { "value": "arm", "label": "ARM" }
            ] },
            { "key": "osVersion", "label": "客户端系统版本（可选）", "type": "string", "default": "",
                "trim": true, "max_length": 128, "description": "仅填写真实值；留空不发送该可选头。" }
        ] })
}
fn setting(key: &str, default: &str) -> Result<String> {
    let value = sdk::settings::get(key)?;
    Ok(value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(default)
        .to_string())
}
fn platform() -> Result<&'static str> {
    match sdk::host("runtime.platform", json!({}))?.as_str() {
        Some("win") => Ok("windows"),
        Some("mac") => Ok("macos"),
        Some("linux") => Ok("linux"),
        Some("other") => Ok("unknown"),
        _ => Err(Error::new("主程序未提供有效的系统类型")),
    }
}
fn version() -> Result<String> {
    let value = setting("clientVersion", "v1.1.5")?;
    if value.len() > 32
        || !value.starts_with('v')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Err(Error::http(400, "小浣熊客户端版本设置无效"));
    }
    Ok(value)
}
pub fn page(mobile: bool) -> Result<Value> {
    if mobile {
        return Ok(json!({ "X-Client-Platform": "app-android", "X-Client-Version": "v1.0.3" }));
    }
    Ok(
        json!({ "X-Client-Name": "raccoon-work", "X-Client-Platform": format!("desktop-{}", platform()?), "X-Client-Version": version()? }),
    )
}
pub fn chat() -> Result<Value> {
    let platform = platform()?;
    let configured_arch = setting("clientArch", "auto")?;
    let arch = if configured_arch == "auto" {
        match sdk::host("runtime.architecture", json!({}))?.as_str() {
            Some("x86_64") => "x64",
            Some("aarch64") => "arm64",
            Some("x86") => "x86",
            Some("arm") => "arm",
            Some(_) => "unknown",
            None => return Err(Error::new("主程序未提供有效的系统架构")),
        }
        .to_string()
    } else {
        configured_arch
    };
    if !["x64", "x86", "arm64", "arm", "unknown"].contains(&arch.as_str()) {
        return Err(Error::http(400, "小浣熊客户端架构设置无效"));
    }
    let stainless_arch = match (platform, arch.as_str()) {
        ("windows", "x64") => "other:amd64",
        (_, "x86") => "x32",
        (_, arch) => arch,
    };
    let os = match platform {
        "windows" => "Windows",
        "macos" => "MacOS",
        "linux" => "Linux",
        _ => "Unknown",
    };
    let mut headers = json!({ "User-Agent": "AsyncOpenAI/Python 2.8.0", "X-Stainless-Lang": "python",
        "X-Stainless-Package-Version": "2.8.0", "X-Stainless-OS": os, "X-Stainless-Arch": stainless_arch,
        "X-Stainless-Runtime": "CPython", "X-Stainless-Runtime-Version": "3.12.13",
        "X-Stainless-Async": "async:asyncio", "X-Stainless-Retry-Count": "0",
        "X-Client-Name": "@raccoon/office-desktop", "X-Client-Platform": format!("desktop-{platform}-{arch}"),
        "X-Client-Version": version()?, "X-Client-Channel": "official" });
    let os_version = setting("osVersion", "")?;
    if !os_version.is_empty() {
        if os_version.len() > 128 || !os_version.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
            return Err(Error::http(400, "客户端系统版本必须是可见ASCII文本"));
        }
        headers["X-Client-OS-Version"] = json!(os_version);
    }
    // 无宿主超时读数时不伪造 X-Stainless-Read-Timeout。
    Ok(headers)
}
