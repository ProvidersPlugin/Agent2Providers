//! 纯格式计算与受控随机/时钟；不访问环境、文件、进程或线程。
use agent2api_wasm_sdk::{self as sdk, json, state, Error, Result, Value};
use base64::Engine;
use sha2::{Digest, Sha256};

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter().find_map(|key| value[*key].as_str().map(str::trim).filter(|value| !value.is_empty()))
        .unwrap_or("").to_string()
}
pub fn scalar(value: &Value) -> String {
    match value { Value::String(value) => value.trim().to_string(), Value::Number(value) => value.to_string(),
        Value::Bool(value) => value.to_string(), _ => String::new() }
}
pub fn number(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str().and_then(|text| text.trim().parse().ok())).filter(|value| value.is_finite())
}
pub fn integer(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
}
pub fn truthy(value: &Value) -> bool {
    match value { Value::Null => false, Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(), Value::Array(items) => !items.is_empty(), Value::Object(_) => true }
}
pub fn strip_bearer(value: &str) -> String {
    let mut token = value.trim();
    loop {
        if !token.get(..6).is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer")) { return token.to_string(); }
        let rest = &token[6..];
        if rest.is_empty() { return String::new(); }
        if !rest.starts_with(char::is_whitespace) { return token.to_string(); }
        token = rest.trim_start();
    }
}
pub fn claims(token: &str) -> Value {
    token.split('.').nth(1).and_then(|value| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(value.trim_end_matches('=')).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok()).filter(Value::is_object).unwrap_or(Value::Null)
}
pub fn jwt_expiry(token: &str) -> Option<i64> {
    number(&claims(token)["exp"]).filter(|value| *value > 0.0 && *value <= i64::MAX as f64 / 1000.0)
        .map(|value| (value * 1000.0) as i64)
}
pub fn hash(value: &str) -> String { format!("{:x}", Sha256::digest(value.as_bytes()))[..32].to_string() }
pub fn random(length: usize) -> Result<Vec<u8>> {
    let value = sdk::host("random.bytes", json!({ "length": length }))?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(value.as_str().unwrap_or(""))
        .map_err(|_| Error::new("宿主随机数据编码无效"))?;
    if bytes.len() != length { return Err(Error::new("宿主随机数据长度无效")); }
    Ok(bytes)
}
pub fn random_hex(length: usize) -> Result<String> { Ok(random(length)?.iter().map(|byte| format!("{byte:02x}")).collect()) }
pub fn uuid() -> Result<String> {
    let mut bytes = random(16)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40; bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let text: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &text[..8], &text[8..12], &text[12..16], &text[16..20], &text[20..]))
}
pub fn token_valid(value: &str, optional: bool) -> Result<()> {
    if (!optional && value.is_empty()) || value.len() > super::endpoints::MAX_TOKEN || !printable(value, super::endpoints::MAX_TOKEN)
        || value.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err(Error::http(400, "AutoClaw 凭据缺失或格式无效"));
    }
    Ok(())
}
pub fn printable(value: &str, max: usize) -> bool { value.len() <= max && value.bytes().all(|byte| (32..=126).contains(&byte)) }
pub fn transient(key: &str) -> Result<state::Snapshot> { state::conversation_get("autoclaw-intl-runtime", key, 1) }
pub fn put(key: &str, before: &state::Snapshot, value: Value, ttl: i64) -> Result<bool> {
    state::conversation_compare_exchange("autoclaw-intl-runtime", key, 1, before.revision, value, ttl)
}
pub fn day() -> Result<String> {
    let now = chrono::DateTime::from_timestamp_millis(sdk::now()).ok_or_else(|| Error::new("宿主时钟无效"))?;
    let zone = chrono::FixedOffset::east_opt(8 * 3600).ok_or_else(|| Error::new("时区无效"))?;
    Ok(now.with_timezone(&zone).format("%Y-%m-%d").to_string())
}
pub fn only(value: &Value, keys: &[&str]) -> bool {
    value.as_object().is_some_and(|fields| fields.keys().all(|key| keys.contains(&key.as_str())))
}
