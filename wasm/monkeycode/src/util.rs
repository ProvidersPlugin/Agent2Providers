//! 只投影所需标量；不把上游原始账号／目录响应公开或持久化。

use agent2api_wasm_sdk::{self as sdk, Error, Result, Value};
use sha2::{Digest, Sha256};

pub fn text<'a>(value: &'a Value, keys: &[&str]) -> &'a str {
    keys.iter()
        .find_map(|key| {
            value
                .get(*key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("")
}

pub fn public_text(raw: &str, session: &str, limit: usize) -> String {
    let redacted = if session.is_empty() {
        raw.to_string()
    } else {
        raw.replace(session, "[已隐藏]")
    };
    redacted
        .chars()
        .filter(|ch| !ch.is_control())
        .take(limit)
        .collect()
}

pub fn number(value: &Value, key: &str) -> Option<f64> {
    let raw = value.get(key)?;
    raw.as_f64()
        .or_else(|| raw.as_str()?.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite())
}

pub fn integer(value: &Value, key: &str) -> Option<i64> {
    let raw = value.get(key)?;
    raw.as_i64().or_else(|| raw.as_str()?.trim().parse().ok())
}

pub fn positive_limit(value: &Value, key: &str) -> Option<i64> {
    integer(value, key).filter(|value| *value > 0 && *value <= (1_i64 << 53) - 1)
}

pub fn now() -> Result<i64> {
    sdk::host("clock.now", Value::Null)?
        .as_i64()
        .filter(|at| *at > 0)
        .ok_or_else(|| Error::http(503, "主程序未提供有效时间，未使用本机时钟猜测"))
}

pub fn fingerprint(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

pub fn conflict() -> Error {
    Error::http(409, "MonkeyCode 账号在读取期间发生变化，请重新查询")
        .with_code("monkeycode_account_changed")
}

pub fn page_number(value: &Value, key: &str, default: u32, max: u32) -> Result<u32> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(raw) => raw
            .as_u64()
            .and_then(|number| u32::try_from(number).ok())
            .filter(|number| (1..=max).contains(number))
            .ok_or_else(|| Error::http(400, "MonkeyCode 分页参数必须是有效的正整数")),
    }
}
