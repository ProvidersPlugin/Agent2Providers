use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use sha2::{Digest, Sha256};

pub fn now() -> Result<i64> {
    sdk::host("clock.now", Value::Null)?
        .as_i64()
        .filter(|time| *time > 0)
        .ok_or_else(|| Error::http(503, "主程序未提供有效时间"))
}

pub fn random(length: usize) -> Result<Vec<u8>> {
    let value = sdk::host("random.bytes", json!({ "length": length }))?;
    let bytes = STANDARD
        .decode(value.as_str().unwrap_or(""))
        .map_err(|_| Error::http(503, "主程序随机数据无效"))?;
    if bytes.len() != length {
        return Err(Error::http(503, "主程序随机数据长度无效"));
    }
    Ok(bytes)
}

pub fn uuid() -> Result<String> {
    let mut bytes = random(16)?;
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

pub fn verifier() -> Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(random(32)?))
}
pub fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// 与旧 flight::fingerprint 完全相同：SHA-256 前 16 字节的 32 位十六进制。
pub fn fingerprint(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn md5(value: &str) -> String {
    format!("{:x}", md5::Md5::digest(value.as_bytes()))
}

pub fn query(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            output.push(char::from(byte));
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

pub fn timestamp(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    let number = value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse::<i64>().ok());
    let time = if let Some(number) = number.filter(|number| *number > 0) {
        if number < 100_000_000_000 {
            number.checked_mul(1000)?
        } else {
            number
        }
    } else {
        chrono::DateTime::parse_from_rfc3339(value.as_str()?)
            .ok()?
            .timestamp_millis()
    };
    (time > 0 && time <= 253_402_300_799_999).then_some(time)
}

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| match value.get(*key) {
            Some(Value::String(text)) if !text.trim().is_empty() => Some(text.trim().to_string()),
            Some(Value::Number(number)) => Some(number.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}
