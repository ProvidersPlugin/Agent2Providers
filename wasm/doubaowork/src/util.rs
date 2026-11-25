//! 纯值转换：时间只从宿主取得；未知读数不补零，不使用本机状态。

use agent2api_wasm_sdk::{self as sdk, Result, Value};
use sha2::{Digest, Sha256};

use crate::errors;

pub fn text<'a>(value: &'a Value, keys: &[&str]) -> &'a str {
    keys.iter()
        .find_map(|key| {
            value
                .get(*key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
        })
        .unwrap_or("")
}
pub fn integer(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse().ok())
}
pub fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|number| number.is_finite() && *number >= 0.0)
}
pub fn unsigned(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.trim().parse().ok())
}
pub fn identifier(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_u64().map(|id| id.to_string()))
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 256
                && id.trim() == id
                && !id.chars().any(|ch| ch.is_control() || ch.is_whitespace())
        })
}
pub fn is_numeric_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.bytes().any(|byte| byte != b'0')
}
pub fn now() -> Result<i64> {
    sdk::host("clock.now", Value::Null)
        .map_err(errors::host)?
        .as_i64()
        .filter(|at| *at > 0 && *at <= 253_402_300_799_999)
        .ok_or_else(|| errors::protocol("主程序未提供有效时间，豆包工作未使用猜测时钟"))
}
pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn encode(value: &str) -> String {
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

/// end_time 的原生口径是毫秒；MIT 固定参考转秒展示时显式 /1000。
/// 不猜秒/微秒单位：只有有效毫秒日期才交给宿主；缺失、0、越界留空。
pub fn timestamp_ms(value: &Value) -> Option<i64> {
    integer(value).filter(|at| (946_684_800_000..=253_402_300_799_999).contains(at))
}

pub fn protected(raw: &str, secrets: &[String]) -> bool {
    secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .any(|secret| raw.contains(secret))
}

pub fn sensitive_key(key: &str) -> bool {
    let key = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect::<String>();
    key.contains("cookie")
        || key.contains("secret")
        || key.contains("password")
        || key.contains("authorization")
        || key.contains("credential")
        || key.contains("signature")
        || matches!(
            key.as_str(),
            "token"
                | "accesstoken"
                | "refreshtoken"
                | "idtoken"
                | "sessiontoken"
                | "authtoken"
                | "uploadauthtoken"
                | "devicetoken"
                | "apikey"
                | "auth"
                | "sessionid"
                | "sessionidss"
                | "sessionkey"
                | "accesskey"
                | "deviceid"
                | "iid"
                | "webid"
                | "fp"
                | "fingerprint"
                | "svwebid"
                | "teauuid"
                | "webtabid"
                | "decision"
        )
}

/// 私有协议元数据保留非凭据字段；上游意外夹带认证信息时拒绝保存而非公开或删改后冒充完整。
pub fn private_metadata(value: &Value, secrets: &[String], depth: usize) -> bool {
    if depth > 16 {
        return false;
    }
    match value {
        Value::Object(fields) => fields
            .iter()
            .all(|(key, value)| !sensitive_key(key) && private_metadata(value, secrets, depth + 1)),
        Value::Array(items) => {
            items.len() <= 512
                && items
                    .iter()
                    .all(|value| private_metadata(value, secrets, depth + 1))
        }
        Value::String(raw) => {
            if protected(raw, secrets) || raw.contains("://") {
                return false;
            }
            let trimmed = raw.trim();
            if trimmed.starts_with('{') || trimmed.starts_with('[') {
                return serde_json::from_str::<Value>(trimmed)
                    .ok()
                    .is_some_and(|value| private_metadata(&value, secrets, depth + 1));
            }
            true
        }
        _ => true,
    }
}

pub fn safe_text(raw: &str, secrets: &[String], limit: usize) -> String {
    if protected(raw, secrets) {
        return "[已隐藏]".into();
    }
    let lower = raw.to_ascii_lowercase();
    if raw.contains("://")
        || lower.contains("sessionid")
        || lower.contains("cookie:")
        || lower.contains("bearer ")
        || lower.contains("lark_suite_access_token")
        || raw.trim_start().starts_with('{')
        || raw.trim_start().starts_with('[')
    {
        return "[已隐藏]".into();
    }
    raw.chars()
        .filter(|ch| !ch.is_control())
        .take(limit)
        .collect::<String>()
        .trim()
        .into()
}

pub fn safe_url(raw: &str, secrets: &[String]) -> Value {
    if raw.len() > 2048 || protected(raw, secrets) {
        return Value::Null;
    }
    let Ok(url) = url::Url::parse(raw) else {
        return Value::Null;
    };
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port_or_known_default() != Some(443)
    {
        return Value::Null;
    }
    Value::String(url.to_string())
}

pub fn object_data(value: &Value, message: &str) -> Result<Value> {
    let data = value.get("data").unwrap_or(value);
    if !data.is_object() {
        return Err(errors::protocol(message));
    }
    Ok(data.clone())
}
