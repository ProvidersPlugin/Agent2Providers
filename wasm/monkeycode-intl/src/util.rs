//! 标量白名单、受控时钟与摘要；不读取系统时间、文件、环境变量。

use agent2api_wasm_sdk::{self as sdk, Error, Result, Value};
use sha2::{Digest, Sha256};

pub const MAX_SAFE_INTEGER: i64 = (1_i64 << 53) - 1;

pub fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// 非空别名必须是文本；空值不是有效身份。错误不携带原值。
pub fn optional_text<'a>(value: &'a Value) -> Result<Option<&'a str>> {
    match value {
        Value::Null => Ok(None),
        Value::String(value) if value.trim().is_empty() => Ok(None),
        Value::String(value) if !value.chars().any(char::is_control) => Ok(Some(value.trim())),
        _ => Err(Error::http(
            400,
            "MonkeyCode 国际导入字段格式无效，未回显原始内容",
        )),
    }
}

pub fn consensus(slot: &mut Option<String>, value: String) -> Result<()> {
    if slot.as_ref().is_some_and(|previous| previous != &value) {
        return Err(Error::http(
            400,
            "MonkeyCode 国际导入存在互相矛盾的凭据或身份字段，未任选一项",
        )
        .with_code("monkeycode_intl_input_conflict"));
    }
    *slot = Some(value);
    Ok(())
}

pub fn uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
        && value
            .bytes()
            .any(|byte| matches!(byte, b'1'..=b'9' | b'a'..=b'f' | b'A'..=b'F'))
}

pub fn number(value: &Value, key: &str) -> Option<f64> {
    let value = value.get(key)?;
    let parsed = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) if !text.trim().is_empty() && text.len() <= 64 => {
            text.trim().parse::<f64>().ok()
        }
        _ => None,
    }?;
    (parsed.is_finite() && parsed.abs() <= MAX_SAFE_INTEGER as f64).then_some(parsed)
}

pub fn nonnegative(value: &Value, key: &str) -> Option<f64> {
    number(value, key).filter(|number| *number >= 0.0)
}

pub fn integer(value: &Value, key: &str) -> Option<i64> {
    value
        .get(key)?
        .as_i64()
        .filter(|number| (-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(number))
}

pub fn page_number(params: &Value, key: &str, default: u32, max: u32) -> Result<u32> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .and_then(|number| u32::try_from(number).ok())
            .filter(|number| (1..=max).contains(number))
            .ok_or_else(|| Error::http(400, "MonkeyCode 国际分页参数无效")),
    }
}

pub fn now() -> Result<i64> {
    sdk::host("clock.now", Value::Null)?
        .as_i64()
        .filter(|at| *at > 0 && *at < MAX_SAFE_INTEGER)
        .ok_or_else(|| {
            Error::http(503, "主程序未提供有效时间，未猜测本机时间")
                .with_code("monkeycode_intl_clock_unavailable")
        })
}

pub fn fingerprint(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

pub fn changed() -> Error {
    Error::http(
        409,
        "MonkeyCode 国际账号在读取期间发生变化或已停用，请重新查询",
    )
    .with_code("monkeycode_intl_account_changed")
}
