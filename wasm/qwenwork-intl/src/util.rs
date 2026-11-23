//! 纯格式处理与宿主受控随机、时钟、账号状态；无文件、环境、进程或原生辅助访问。
use agent2api_wasm_sdk::{self as sdk, json, state, Error, Result, Value};
use base64::Engine;
use sha2::{Digest, Sha256};

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| match value.get(*key) {
            Some(Value::String(text)) if !text.trim().is_empty() => Some(text.trim().to_string()),
            Some(Value::Number(number)) => Some(number.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}
pub fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        .filter(|number| number.is_finite())
}
pub fn integer(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
}
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}
pub fn printable(value: &str, max: usize) -> bool {
    value.len() <= max && value.bytes().all(|byte| (32..=126).contains(&byte))
}
pub fn identity(value: &str) -> Result<()> {
    if !printable(value, 256)
        || value.contains('|')
        || value.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return Err(Error::http(400, "千问办公用户或设备标识格式无效"));
    }
    Ok(())
}
pub fn secret(value: &Value, keys: &[&str]) -> Result<String> {
    let mut raw = keys
        .iter()
        .find_map(|key| {
            value[*key]
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or("");
    while raw
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Bearer "))
    {
        raw = raw[7..].trim();
    }
    if !printable(raw, super::endpoints::MAX_TOKEN)
        || raw.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return Err(Error::http(400, "千问办公凭据过长或格式无效"));
    }
    Ok(raw.into())
}
pub fn timestamp(value: &Value) -> Option<i64> {
    if let Some(number) = integer(value).filter(|number| *number > 0) {
        return if number < 100_000_000_000 {
            number.checked_mul(1000)
        } else {
            Some(number)
        };
    }
    chrono::DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|time| time.timestamp_millis())
        .filter(|time| *time > 0)
}
pub fn token_expiry(value: &Value, access: &str) -> Result<Option<i64>> {
    for key in [
        "expiresAt",
        "expires",
        "expires_at",
        "tokenExpiresAt",
        "expire_time",
        "expireTime",
    ] {
        if let Some(raw) = value.get(key).filter(|raw| !raw.is_null() && *raw != "") {
            return timestamp(raw)
                .map(Some)
                .ok_or_else(|| Error::http(502, "千问办公返回了无效的令牌到期时刻"));
        }
    }
    if let Some(part) = access
        .split('.')
        .nth(1)
        .filter(|part| part.len() <= 128 * 1024)
    {
        if let Ok(bytes) =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(part.trim_end_matches('='))
        {
            if let Ok(jwt) = serde_json::from_slice::<Value>(&bytes) {
                if let Some(exp) = integer(&jwt["exp"])
                    .filter(|exp| *exp > 0)
                    .and_then(|exp| exp.checked_mul(1000))
                {
                    // JWT这里只作有效时刻提示，不替代在线账号身份或签名校验。
                    return Ok(Some(exp));
                }
            }
        }
    }
    Ok(None)
}
pub fn now() -> Result<i64> {
    let value = sdk::host("clock.now", Value::Null)?;
    value
        .as_i64()
        .filter(|value| *value > 0)
        .ok_or_else(|| Error::http(503, "主程序时钟不可用"))
}
pub fn random<const N: usize>() -> Result<[u8; N]> {
    if N == 0 || N > 256 {
        return Err(Error::new("随机字节长度超出约定"));
    }
    let value = sdk::host("random.bytes", json!({ "length": N }))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.as_str().unwrap_or(""))
        .map_err(|_| Error::new("主程序随机数据无效"))?;
    bytes
        .try_into()
        .map_err(|_| Error::new("主程序随机数据长度不符"))
}
pub fn random_secret() -> Result<String> {
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random::<32>()?))
}
pub fn uuid() -> Result<String> {
    let mut bytes = random::<16>()?;
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let raw: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &raw[..8],
        &raw[8..12],
        &raw[12..16],
        &raw[16..20],
        &raw[20..]
    ))
}
pub fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
pub fn transient(key: &str) -> Result<state::Snapshot> {
    if key == "refresh-control" {
        state::get(state::Scope::Account, "qwenwork-intl-refresh-control", 1)
    } else {
        state::conversation_get("qwenwork-intl-runtime", key, 1)
    }
}
pub fn put(key: &str, before: &state::Snapshot, value: Value, ttl: i64) -> Result<bool> {
    if key == "refresh-control" {
        state::compare_exchange(
            state::Scope::Account,
            "qwenwork-intl-refresh-control",
            1,
            before.revision,
            value,
            0,
        )
    } else {
        state::conversation_compare_exchange(
            "qwenwork-intl-runtime",
            key,
            1,
            before.revision,
            value,
            ttl,
        )
    }
}
pub fn terminal(error: &Error) -> bool {
    matches!(
        error.code.as_deref(),
        Some(
            "forbidden"
                | "aborted"
                | "canceled"
                | "cancelled"
                | "identity_changed"
                | "capability_not_verified"
        )
    )
}
