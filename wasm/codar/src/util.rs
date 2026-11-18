use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use base64::{engine::general_purpose::STANDARD, Engine};
use sha2::{Digest, Sha256};

pub fn now() -> Result<i64> {
    let now = sdk::now();
    if now <= 0 {
        Err(Error::new("主程序未提供有效时间，未执行科搭操作"))
    } else {
        Ok(now)
    }
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
    let text = if let Some(text) = value.as_str() {
        text.trim().to_string()
    } else if value.is_u64() || value.is_i64() {
        value.to_string()
    } else {
        return None;
    };
    (!text.is_empty() && text.len() <= 256 && !text.chars().any(char::is_control)).then_some(text)
}

pub fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|value| value.is_finite())
}

pub fn amount(value: &Value) -> Option<f64> {
    number(value).filter(|value| *value >= 0.0)
}

pub fn timestamp(value: &Value) -> Option<i64> {
    if let Some(number) = number(value).filter(|number| *number > 0.0) {
        let millis = if number < 10_000_000_000.0 {
            number * 1000.0
        } else {
            number
        };
        return (millis <= 253_402_300_799_999.0).then_some(millis as i64);
    }
    let text = value.as_str()?;
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.timestamp_millis())
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|time| time.and_utc().timestamp_millis())
        })
        .filter(|time| *time > 0 && *time <= 253_402_300_799_999)
}

pub fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

pub fn nonce() -> Result<String> {
    let value = sdk::host("random.bytes", json!({ "length": 24 }))?;
    let bytes = STANDARD
        .decode(value.as_str().unwrap_or(""))
        .map_err(|_| Error::new("主程序随机数返回格式无效"))?;
    if bytes.len() != 24 {
        return Err(Error::new("主程序随机数长度无效"));
    }
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn safe_text(text: &str, protected: &[String], limit: usize) -> String {
    let mut text = text.to_string();
    let mut secrets = protected
        .iter()
        .filter(|secret| !secret.is_empty())
        .collect::<Vec<_>>();
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    for secret in secrets {
        text = text.replace(secret, "[已隐藏]");
    }
    let text: String = text.chars().take(8192).collect();
    let text = match regex::Regex::new(r"eyJ[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+){0,2}") {
        Ok(pattern) => pattern.replace_all(&text, "[令牌已隐藏]").into_owned(),
        Err(_) => "科搭返回了无法安全显示的信息".into(),
    };
    text.chars()
        .filter(|ch| {
            !ch.is_control() && !matches!(*ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(limit)
        .collect()
}

pub fn redact(mut error: Error, protected: &[String]) -> Error {
    error.message = safe_text(&error.message, protected, 600);
    error
}

pub fn display(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0} 点")
    } else {
        format!("{value:.2} 点")
    }
}

pub fn checked_url(raw: &str) -> Result<url::Url> {
    let url = url::Url::parse(raw).map_err(|_| Error::http(502, "科搭返回的地址无效"))?;
    if raw.len() > 8192
        || url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::http(502, "科搭返回的地址不符合 HTTPS 安全要求"));
    }
    Ok(url)
}
