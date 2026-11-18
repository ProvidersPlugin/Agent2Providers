//! 纯数据工具；时钟和随机数只从受控 SDK 取得，不访问操作系统。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sha2::{Digest, Sha256};

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| {
            value
                .get(*key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
        })
        .unwrap_or("")
        .to_string()
}

pub fn random(length: usize) -> Result<Vec<u8>> {
    let value = sdk::host("random.bytes", json!({ "length": length }))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.as_str().unwrap_or(""))
        .map_err(|_| Error::new("主程序随机数编码无效"))?;
    if bytes.len() != length {
        return Err(Error::new("主程序随机数字节数无效"));
    }
    Ok(bytes)
}

pub fn random_hex(length: usize) -> Result<String> {
    Ok(hex(&random(length)?))
}
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
pub fn sha256(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}
pub fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// 与旧 trial 台账的 flight::fingerprint 逐字一致，不能暴露到日志或公开账号。
pub fn fingerprint(text: &str) -> String {
    sha256(text.as_bytes())[..32].to_string()
}

pub fn encode(text: &str) -> String {
    let mut encoded = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

pub fn parse_ms(value: &Value) -> i64 {
    value
        .as_i64()
        .or_else(|| {
            value.as_str().and_then(|text| {
                chrono::DateTime::parse_from_rfc3339(text.trim())
                    .ok()
                    .map(|date| date.timestamp_millis())
            })
        })
        .unwrap_or(0)
}

pub fn day(at: i64) -> String {
    chrono::DateTime::from_timestamp_millis(at.saturating_add(8 * 3600 * 1000))
        .map(|date| date.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

pub fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|value| value.is_finite())
}

pub fn warning(value: &mut Value, message: &str) {
    if !value["warnings"].is_array() {
        value["warnings"] = json!([]);
    }
    if let Some(warnings) = value["warnings"].as_array_mut() {
        warnings.push(json!(message));
    }
}
