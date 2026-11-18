//! 本包只拥有国际版身份。ZCode 登录/套餐平面两地同域，Z.AI 业务/推理平面独立。
//! 网络只经受控 SDK；授权页、端点映射也分别按本家的已知来源检查。
use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};
use base64::Engine;
use sha2::{Digest, Sha256};

pub const PROVIDER: &str = "zcode-intl";
pub const ID_PREFIX: &str = "zcode-intl-user-";
pub const ZCODE: &str = "https://zcode.z.ai";
pub const OPENAI: &str = "https://api.z.ai/api/coding/paas/v4";
pub const MONITOR: &str = "https://api.z.ai";
pub const BIZ: &str = "https://api.z.ai";
pub const AUTHORIZE: &str = "https://chat.z.ai";

pub struct Reply {
    pub status: u16,
    pub value: Value,
}

pub fn request(method: &str, url: &str, headers: Value, body: Option<&Value>) -> Result<Reply> {
    let response = http::open_timeout(method, url, headers, body, 15_000)?;
    let status = response.status;
    let bytes = response.bytes(4 * 1024 * 1024)?;
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Ok(Reply { status, value })
}

pub fn envelope(reply: Reply, operation: &str) -> Result<Value> {
    let code = integer(&reply.value["code"]);
    if reply.status == 401 || code == Some(401) {
        return Err(Error::http(
            401,
            format!("{operation}：ZCode 国际版登录态已失效，请重新登录"),
        )
        .with_code("authentication_error"));
    }
    if !(200..300).contains(&reply.status)
        || !matches!(code, Some(0 | 200))
        || reply.value["success"] == false
    {
        let mut error = Error::http(
            502,
            format!(
                "{operation}未完成（HTTP {}，业务码 {}）",
                reply.status,
                code.map(|value| value.to_string())
                    .unwrap_or_else(|| "未返回".into())
            ),
        );
        error.upstream_code = code;
        return Err(error);
    }
    Ok(reply.value.get("data").cloned().unwrap_or(Value::Null))
}

pub fn query(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| {
            value[*key]
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or("")
        .to_string()
}

pub fn integer(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
}

pub fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        .filter(|value| value.is_finite())
}

pub fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))[..32].to_string()
}

pub fn random(length: usize) -> Result<Vec<u8>> {
    let value = sdk::host("random.bytes", json!({ "length": length }))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.as_str().unwrap_or(""))
        .map_err(|_| Error::new("宿主随机数据编码无效"))?;
    if bytes.len() != length {
        return Err(Error::new("宿主随机数据长度无效"));
    }
    Ok(bytes)
}

pub fn random_hex(length: usize) -> Result<String> {
    Ok(random(length)?
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub fn uuid() -> Result<String> {
    let mut bytes = random(16)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

pub fn uuid_shape(value: &str) -> bool {
    let parts = value.split('-').collect::<Vec<_>>();
    parts.len() == 5
        && parts.iter().zip([8, 4, 4, 4, 12]).all(|(part, length)| {
            part.len() == length && part.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

// 领取窗口来自共用 ZCode 活动平面，不按浏览器/部署所在地推测；上游活动时间优先。
pub fn beijing_day(time: i64) -> i64 {
    (time + 28_800_000).div_euclid(86_400_000)
}
pub fn same_day(time: i64, now: i64) -> bool {
    time > 0 && beijing_day(time) == beijing_day(now)
}
pub fn daily_window(time: i64) -> i64 {
    beijing_day(time) * 86_400_000 + 19 * 3_600_000 - 28_800_000
}

pub fn clock_label(time: i64) -> Option<String> {
    let zone = chrono::FixedOffset::east_opt(8 * 3600)?;
    Some(
        chrono::DateTime::from_timestamp_millis(time)?
            .with_timezone(&zone)
            .format("%m-%d %H:%M")
            .to_string(),
    )
}

pub fn compact(value: f64) -> String {
    let (value, unit) = if value.abs() >= 100_000_000.0 {
        (value / 100_000_000.0, "亿")
    } else if value.abs() >= 10_000.0 {
        (value / 10_000.0, "万")
    } else {
        (value, "")
    };
    format!(
        "{}{unit}",
        format!("{value:.2}")
            .trim_end_matches('0')
            .trim_end_matches('.')
    )
}
