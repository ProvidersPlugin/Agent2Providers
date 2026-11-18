//! WASM 只使用主程序的时钟/随机源；不读环境、不使用系统随机或全局缓存。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use base64::Engine;
use sha2::{Digest, Sha256};

pub fn now() -> Result<i64> {
    sdk::host("clock.now", Value::Null)?
        .as_i64()
        .filter(|at| *at > 0)
        .ok_or_else(|| Error::coded("主程序时钟不可用", "clock_unavailable"))
}

pub fn random_hex(length: usize) -> Result<String> {
    let value = sdk::host("random.bytes", json!({ "length": length }))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.as_str().unwrap_or(""))
        .map_err(|_| Error::new("主程序随机数编码无效"))?;
    if bytes.len() != length {
        return Err(Error::new("主程序随机数长度不符"));
    }
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn fingerprint(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

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

pub fn integer(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse().ok())
}

pub fn safe_text(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .take(limit)
        .collect()
}

pub fn header(text: &str, limit: usize) -> Result<String> {
    if text.len() > limit || text.chars().any(char::is_control) {
        return Err(Error::http(400, "Antigravity 凭据或请求头字段格式无效"));
    }
    Ok(text.trim().to_string())
}

pub fn tail(text: &str) -> String {
    text.chars()
        .skip(text.chars().count().saturating_sub(4))
        .collect()
}

pub fn rfc3339(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text.trim())
        .ok()
        .map(|at| at.timestamp_millis())
}

pub fn quota_fraction(value: &Value) -> Option<f64> {
    let raw = value
        .get("remainingFraction")
        .or_else(|| value.get("remaining_fraction"));
    let has_amount = value
        .get("remainingAmount")
        .or_else(|| value.get("remaining_amount"))
        .is_some_and(|v| !v.is_null());
    let reset = text(value, &["resetTime", "reset_time"]);
    match raw {
        Some(fraction) if !fraction.is_null() => fraction
            .as_f64()
            .filter(|fraction| fraction.is_finite() && (0.0..=1.0).contains(fraction)),
        _ if !has_amount && rfc3339(&reset).is_some() => Some(0.0),
        _ => None,
    }
}

pub fn conflict() -> Error {
    Error::http(409, "Antigravity 账号已在操作期间变化，未覆盖新的凭据")
        .with_code("credential_conflict")
}
