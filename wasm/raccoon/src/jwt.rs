//! 只解JWT声明，不用未验签声明做授权。旧sid/iss十六进制转十进制身份保持原规则。
//! IAM issuer=小浣熊/iam，只认官方issuer、非空sub和真实exp；平台uid仍需user_info获取。

use super::util;
use agent2api_wasm_sdk::{self as sdk, Error, Result, Value};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

pub fn strip(raw: &str) -> String {
    let mut value = raw.trim();
    loop {
        let Some(prefix) = value.get(..6) else {
            return value.to_string();
        };
        if !prefix.eq_ignore_ascii_case("bearer") {
            return value.to_string();
        }
        let rest = &value[6..];
        if rest.is_empty() {
            return String::new();
        }
        if rest.trim_start().len() == rest.len() {
            return value.to_string();
        }
        value = rest.trim_start();
    }
}

pub fn claims(token: &str) -> Option<Value> {
    let token = strip(token);
    if token.len() > super::endpoints::MAX_CREDENTIALS {
        return None;
    }
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    if payload.is_empty() {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .filter(Value::is_object)
}
pub fn iam(token: &str) -> bool {
    claims(token).is_some_and(|claims| {
        claims["iss"]
            .as_str()
            .is_some_and(|issuer| issuer.ends_with("/iam"))
    })
}
pub fn expiry(token: &str) -> Option<i64> {
    util::number(&claims(token)?["exp"])
        .map(|value| value * 1000.0)
        .filter(|value| value.is_finite() && *value > 0.0 && *value < i64::MAX as f64)
        .map(|value| value as i64)
}
pub fn validate_iam(token: &str) -> Result<()> {
    let claims = claims(token).unwrap_or(Value::Null);
    if claims["iss"] == "xiaohuanxiong.com/iam"
        && claims["sub"]
            .as_str()
            .is_some_and(|sub| !sub.trim().is_empty())
        && expiry(token).is_some_and(|at| at > sdk::now())
    {
        return Ok(());
    }
    Err(Error::http(
        401,
        "小浣熊 IAM 登录态无效或已过期，请重新网页登录；不能使用旧刷新接口续期",
    )
    .with_code("raccoon_iam_relogin"))
}

pub fn uid(claims: &Value) -> String {
    if claims["iss"]
        .as_str()
        .is_some_and(|issuer| issuer.ends_with("/iam"))
    {
        return util::text(claims, &["sub"]);
    }
    for key in ["sid", "iss"] {
        if let Some(value) = decimal(&claims[key]) {
            return value;
        }
    }
    for key in ["sub", "id", "user_id"] {
        let value = util::js(&claims[key]);
        if !value.trim().is_empty() {
            return value.trim().to_string();
        }
    }
    String::new()
}
fn decimal(value: &Value) -> Option<String> {
    let raw = match value {
        Value::String(value) => value.trim().to_string(),
        Value::Number(value) => value.to_string(),
        _ => return None,
    };
    let raw = raw
        .strip_prefix("0x")
        .or_else(|| raw.strip_prefix("0X"))
        .unwrap_or(&raw);
    if raw.is_empty() || raw.len() > 256 || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut digits = vec![0u8];
    for byte in raw.bytes() {
        let mut carry = char::from(byte).to_digit(16)?;
        for digit in &mut digits {
            let next = u32::from(*digit) * 16 + carry;
            *digit = (next % 10) as u8;
            carry = next / 10;
        }
        while carry > 0 {
            digits.push((carry % 10) as u8);
            carry /= 10;
        }
    }
    let decimal: String = digits
        .iter()
        .rev()
        .map(|digit| char::from(b'0' + *digit))
        .collect();
    Some(if decimal.trim_start_matches('0').is_empty() {
        "0".into()
    } else {
        decimal.trim_start_matches('0').into()
    })
}
