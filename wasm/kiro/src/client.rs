//! 原生Kiro客户端头保留版本/算法，机器摘要与平台/架构只能取主程序真实事实。
//! runtime.client_fingerprint只返回SHA256("agent2api:"+真实主机名)，不会暴露环境变量。

use crate::util;
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

fn platform() -> Result<&'static str> {
    match sdk::host("runtime.platform", json!({}))?.as_str() {
        Some("win") => Ok("win32"),
        Some("mac") => Ok("darwin"),
        Some("linux") => Ok("linux"),
        _ => Err(Error::http(503, "主程序未提供Kiro需要的真实系统类型")),
    }
}
fn architecture() -> Result<String> {
    let value = sdk::host("runtime.architecture", json!({}))?;
    let value = value
        .as_str()
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        })
        .ok_or_else(|| Error::http(503, "主程序未提供有效主机架构"))?;
    Ok(match value {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
    .into())
}
pub fn ide_headers() -> Result<Value> {
    let value = sdk::host("runtime.client_fingerprint", json!({}))?;
    let fingerprint = value
        .as_str()
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| Error::http(503, "主程序未提供Kiro需要的真实客户端摘要"))?;
    Ok(
        json!({ "Accept": "application/json", "Content-Type": "application/json",
        "User-Agent": format!("KiroIDE-1.1.70-{fingerprint}") }),
    )
}
pub fn social_headers() -> Result<Value> {
    Ok(
        json!({ "Accept": "application/json", "Content-Type": "application/json",
        "User-Agent": format!("Kiro-Desktop/0.2.13 ({}; {})", platform()?, architecture()?) }),
    )
}
pub fn streaming_agent() -> Result<String> {
    let platform = match platform()? {
        "win32" => "windows",
        "darwin" => "macos",
        other => other,
    };
    let message = util::message_id()?;
    Ok(format!("aws-sdk-rust/1.0.0 ua/2.1 os/{platform} lang/rust api/codewhispererstreaming#1.28.3 m/E app/AmazonQ-For-CLI md/appVersion-1.28.3-{message}"))
}
