//! 时钟和随机数只向宿主申请；失败不退回时间戳、固定值或进程内计数。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use sha2::{Digest, Sha256};

pub fn now() -> Result<i64> {
    sdk::host("clock.now", Value::Null)?
        .as_i64()
        .filter(|value| *value > 0)
        .ok_or_else(|| Error::new("主程序没有提供有效时间"))
}
pub fn random(length: usize) -> Result<Vec<u8>> {
    let value = sdk::host("random.bytes", json!({ "length": length }))?;
    let bytes = STANDARD
        .decode(value.as_str().unwrap_or(""))
        .map_err(|_| Error::new("主程序安全随机数格式无效"))?;
    if bytes.len() != length {
        return Err(Error::new("主程序安全随机数长度无效"));
    }
    Ok(bytes)
}
pub fn nonce(length: usize) -> Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(random(length)?))
}
pub fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}
pub fn hash(value: &str, bytes: usize) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .take(bytes)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
pub fn uuid() -> Result<String> {
    let mut bytes = random(16)?;
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let text: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &text[..8],
        &text[8..12],
        &text[12..16],
        &text[16..20],
        &text[20..]
    ))
}
pub fn message_id() -> Result<String> {
    let mut bytes = random(16)?;
    let millis = now()? as u64;
    for (index, shift) in [40, 32, 24, 16, 8, 0].into_iter().enumerate() {
        bytes[index] = (millis >> shift) as u8;
    }
    bytes[6] = (bytes[6] & 15) | 112;
    bytes[8] = (bytes[8] & 63) | 128;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|text| !text.is_empty())
        .unwrap_or("")
        .to_string()
}
pub fn timestamp(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    let number = value
        .as_i64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()));
    if let Some(number) = number.filter(|number| *number > 0) {
        return if number < 10_000_000_000 {
            number.checked_mul(1000)
        } else {
            Some(number)
        };
    }
    chrono::DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|date| date.timestamp_millis())
}
pub fn number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        })
        .filter(|value| value.is_finite() && *value >= 0.0)
}
pub fn query(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
pub fn bounded(value: &str, bytes: usize) -> String {
    let mut end = value.len().min(bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}
pub fn identifier(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}
