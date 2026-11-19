//! 仅宿主时钟/随机源；UUIDv7请求形态、JWT只读声明与固定北京时间。
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use chrono::{FixedOffset, TimeZone};
use sha2::{Digest, Sha256};

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| {
            value
                .get(*key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("")
        .into()
}
pub fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|n| n.is_finite())
}
pub fn integer(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| {
        number(value)
            .filter(|n| *n >= i64::MIN as f64 && *n < i64::MAX as f64)
            .map(|n| n.floor() as i64)
    })
}
pub fn truth(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(_) => number(value).is_some_and(|n| n != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}
pub fn hash(value: &str) -> String {
    hex(&Sha256::digest(value.as_bytes()))
}
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
/// 边序列化边摘要：用于需要把一段 JSON 做哈希、但不该在内存里再拼一份副本的场景。
pub struct Hasher(Sha256);
impl Hasher {
    pub fn new() -> Self {
        Self(Sha256::new())
    }
    pub fn finish(self) -> String {
        hex(&self.0.finalize())
    }
}
impl std::io::Write for Hasher {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub fn random(size: usize) -> Result<Vec<u8>> {
    let encoded = sdk::host("random.bytes", json!({ "length": size }))?;
    let bytes = STANDARD
        .decode(encoded.as_str().unwrap_or(""))
        .map_err(|_| Error::new("主程序随机源编码无效"))?;
    if bytes.len() != size {
        return Err(Error::new("主程序随机源长度不符"));
    }
    Ok(bytes)
}
pub fn random_hex(size: usize) -> Result<String> {
    Ok(hex(&random(size)?))
}
pub fn message_id() -> Result<String> {
    let mut bytes = random(16)?;
    bytes[..6].copy_from_slice(&sdk::now().to_be_bytes()[2..]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(hex(&bytes))
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
pub fn strip(raw: &str) -> String {
    let mut raw = raw.trim();
    loop {
        let Some(prefix) = raw.get(..6) else { break };
        if !prefix.eq_ignore_ascii_case("bearer") {
            break;
        }
        let rest = &raw[6..];
        if rest.is_empty() {
            return String::new();
        }
        if rest.trim_start().len() == rest.len() {
            break;
        }
        raw = rest.trim_start();
    }
    raw.into()
}
pub fn claims(raw: &str) -> Option<Value> {
    let raw = strip(raw);
    let payload = raw.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice::<Value>(&decoded)
        .ok()
        .filter(Value::is_object)
}
pub fn expires(auth: &Value, absolute: &str, relative: &str, now: i64) -> Option<i64> {
    integer(&auth[absolute]).filter(|n| *n > 0).or_else(|| {
        integer(&auth[relative])
            .filter(|n| *n > 0)
            .map(|seconds| now.saturating_add(seconds.saturating_mul(1000)))
    })
}
pub fn day(at: i64) -> String {
    chrono::DateTime::from_timestamp_millis(at.saturating_add(28_800_000))
        .map(|v| v.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}
pub fn time(value: &Value) -> Option<i64> {
    if let Some(s) = value.as_str() {
        if let Ok(v) = chrono::DateTime::parse_from_rfc3339(s) {
            return Some(v.timestamp_millis());
        }
        if let Ok(v) = chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d") {
            return v
                .and_hms_opt(0, 0, 0)
                .map(|v| v.and_utc().timestamp_millis());
        }
        if let Ok(v) = chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S")
            .or_else(|_| chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%dT%H:%M:%S"))
        {
            return FixedOffset::east_opt(28_800)?
                .from_local_datetime(&v)
                .single()
                .map(|v| v.timestamp_millis());
        }
    }
    integer(value).filter(|v| *v > 0).map(|v| {
        if v < 100_000_000_000 {
            v.saturating_mul(1000)
        } else {
            v
        }
    })
}
pub fn tail(s: &str) -> String {
    s.chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}
pub fn warning(value: &mut Value, message: &str) {
    if !value["warnings"].is_array() {
        value["warnings"] = json!([]);
    }
    if let Some(rows) = value["warnings"].as_array_mut() {
        rows.push(json!(message));
    }
}
