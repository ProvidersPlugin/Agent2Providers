//! 本包通用值处理；时钟与随机源只有宿主 SDK，不读取环境或文件。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use base64::Engine;
use sha2::{Digest, Sha256};

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| {
            value
                .get(*key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or("")
        .to_string()
}
pub fn js(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}
pub fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|value| value.is_finite())
}
pub fn integer(value: &Value) -> Option<i64> {
    number(value)
        .filter(|value| *value >= i64::MIN as f64 && *value < i64::MAX as f64)
        .map(|value| value.floor() as i64)
}
pub fn random(bytes: usize) -> Result<Vec<u8>> {
    let value = sdk::host("random.bytes", json!({ "length": bytes }))?;
    let value = base64::engine::general_purpose::STANDARD
        .decode(value.as_str().unwrap_or(""))
        .map_err(|_| Error::new("主程序随机数编码无效"))?;
    if value.len() != bytes {
        return Err(Error::new("主程序随机数字节数不符"));
    }
    Ok(value)
}
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
pub fn hash(value: &str) -> String {
    hex(&Sha256::digest(value.as_bytes()))
}
pub fn random_hex(bytes: usize) -> Result<String> {
    Ok(hex(&random(bytes)?))
}
pub fn uuid_of(hex: &str) -> String {
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}
pub fn day(at: i64) -> String {
    chrono::DateTime::from_timestamp_millis(at.saturating_add(8 * 3600 * 1000))
        .map(|date| date.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}
pub fn warning(value: &mut Value, message: &str) {
    if !value["warnings"].is_array() {
        value["warnings"] = json!([]);
    }
    if let Some(rows) = value["warnings"].as_array_mut() {
        rows.push(json!(message));
    }
}
pub fn tail(token: &str) -> String {
    token
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}
