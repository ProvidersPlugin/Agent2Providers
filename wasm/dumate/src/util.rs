use agent2api_wasm_sdk::{self as sdk, Error, Result, Value};
use sha2::{Digest, Sha256};

pub fn now() -> Result<i64> {
    let now = sdk::now();
    if now <= 0 {
        return Err(Error::new("主程序未提供有效时间，未执行百度搭子操作"));
    }
    Ok(now)
}

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| {
            value[*key]
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
        })
        .unwrap_or("")
        .to_string()
}

pub fn identifier(value: &Value) -> Option<String> {
    let id = value
        .as_str()
        .map(str::trim)
        .map(str::to_string)
        .or_else(|| value.as_u64().map(|id| id.to_string()))?;
    (!id.is_empty()
        && id.len() <= 256
        && id.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'@')
        }))
    .then_some(id)
}

pub fn number(value: &Value) -> Option<f64> {
    let number = value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())?;
    number.is_finite().then_some(number)
}

pub fn timestamp(value: &Value) -> Option<i64> {
    if let Some(number) = number(value).filter(|number| *number > 0.0) {
        let milliseconds = if number < 100_000_000_000.0 {
            number * 1000.0
        } else {
            number
        };
        return (milliseconds < i64::MAX as f64).then_some(milliseconds as i64);
    }
    chrono::DateTime::parse_from_rfc3339(value.as_str()?.trim())
        .ok()
        .map(|date| date.timestamp_millis())
        .filter(|at| *at > 0)
}

pub fn day_start(now: i64) -> i64 {
    (now + 28_800_000).div_euclid(86_400_000) * 86_400_000 - 28_800_000
}

pub fn date(now: i64) -> Result<String> {
    chrono::DateTime::from_timestamp_millis(now + 28_800_000)
        .map(|date| date.format("%Y-%m-%d").to_string())
        .ok_or_else(|| Error::new("百度搭子签到日期无效"))
}

pub fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

pub fn safe_text(text: &str, protected: &[String], limit: usize) -> String {
    let mut text = text.to_string();
    for secret in protected.iter().filter(|secret| secret.len() >= 8) {
        text = text.replace(secret, "[已隐藏]");
    }
    text.chars()
        .filter(|character| !character.is_control())
        .take(limit)
        .collect()
}

pub fn display(number: f64) -> String {
    if number.fract().abs() < 0.00001 {
        format!("{number:.0}")
    } else {
        format!("{number:.2}")
    }
}
