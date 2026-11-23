//! 时钟与随机只来自 SDK。没有系统随机、环境变量、文件访问或全局状态。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use sha2::{Digest, Sha256};

pub fn now() -> Result<i64> {
    sdk::host("clock.now", Value::Null)?
        .as_i64()
        .filter(|at| *at > 0 && *at < (1_i64 << 53))
        .ok_or_else(|| Error::http(503, "主程序没有提供有效时钟"))
}

pub fn random(count: usize) -> Result<Vec<u8>> {
    if count == 0 || count > 256 {
        return Err(Error::http(500, "随机字节数量超出限制"));
    }
    let value = sdk::host("random.bytes", json!({ "length": count }))?;
    let bytes = STANDARD
        .decode(value.as_str().unwrap_or(""))
        .map_err(|_| Error::http(503, "主程序随机数格式无效"))?;
    if bytes.len() != count {
        return Err(Error::http(503, "主程序随机数长度无效"));
    }
    Ok(bytes)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
pub fn random_hex(count: usize) -> Result<String> {
    Ok(hex(&random(count)?))
}
pub fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}
pub fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn uuid() -> Result<String> {
    let mut bytes = random(16)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let h = hex(&bytes);
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}

pub fn digits(length: usize) -> Result<String> {
    if !(1..=32).contains(&length) {
        return Err(Error::http(500, "设备标识长度无效"));
    }
    let mut out = String::new();
    for _ in 0..16 {
        for byte in random(32)? {
            if byte < 250 {
                out.push(char::from(b'0' + byte % 10));
                if out.len() == length {
                    return Ok(out);
                }
            }
        }
    }
    Err(Error::http(503, "无法生成安全设备标识"))
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

pub fn scalar(value: &Value) -> String {
    value
        .as_str()
        .map(str::trim)
        .map(str::to_string)
        .or_else(|| value.as_i64().map(|v| v.to_string()))
        .or_else(|| value.as_u64().map(|v| v.to_string()))
        .unwrap_or_default()
}

pub fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|v| v.is_finite())
}

pub fn integer(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| {
        number(value)
            .filter(|v| v.abs() <= (1_i64 << 53) as f64)
            .map(|v| v as i64)
    })
}

pub fn millis(value: &Value) -> Option<i64> {
    let v = integer(value).filter(|v| *v > 0)?;
    let v = if v < 100_000_000_000 {
        v.checked_mul(1000)?
    } else {
        v
    };
    (v < (1_i64 << 53)).then_some(v)
}

pub fn safe_text(value: &str, max: usize) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .take(max)
        .collect()
}

pub fn header(value: &str, max: usize) -> Result<String> {
    let value = value.trim();
    if value.len() > max
        || !value.is_ascii()
        || value
            .bytes()
            .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
    {
        return Err(Error::http(400, "Trae 凭据或身份字段格式无效"));
    }
    Ok(value.to_string())
}

pub fn account_origin(raw: &str) -> Result<String> {
    let value = raw.trim().trim_end_matches('/');
    let value = if value.contains("://") {
        value.to_string()
    } else {
        format!("https://{value}")
    };
    if super::endpoints::ACCOUNT_ORIGINS.contains(&value.as_str()) {
        Ok(value)
    } else {
        Err(Error::http(400, "Trae 只接受已确认的国内账号服务地址").with_code("local_precondition"))
    }
}

pub fn account_hosts(primary: &str, include_web: bool) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if !primary.trim().is_empty() {
        out.push(account_origin(primary)?);
    }
    for host in &super::endpoints::ACCOUNT_ORIGINS[..if include_web { 3 } else { 2 }] {
        if !out.iter().any(|seen| seen.as_str() == *host) {
            out.push((*host).into());
        }
    }
    Ok(out)
}
