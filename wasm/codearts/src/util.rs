use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use base64::Engine;
use sha2::{Digest, Sha256};

use crate::errors;

pub fn now() -> Result<i64> {
    let value = sdk::host("clock.now", Value::Null).map_err(errors::host)?;
    value
        .as_i64()
        .filter(|at| *at > 0 && chrono::DateTime::from_timestamp_millis(*at).is_some())
        .ok_or_else(|| errors::protocol("主程序没有提供有效时钟，未签名或修改状态"))
}

pub fn random<const N: usize>() -> Result<[u8; N]> {
    let value = sdk::host("random.bytes", json!({ "length": N })).map_err(errors::host)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.as_str().unwrap_or(""))
        .map_err(|_| errors::protocol("主程序随机数编码无效"))?;
    bytes
        .try_into()
        .map_err(|_| errors::protocol("主程序随机数长度无效"))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn digest(parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    hex(&hash.finalize())
}

pub fn object(text: &str) -> Result<Value> {
    if text.len() > 128 * 1024 {
        return Err(errors::invalid("CodeArts 凭据文本超过 128 KiB"));
    }
    let value: Value = serde_json::from_str(text.trim().trim_start_matches('\u{feff}').trim())
        .map_err(|_| errors::invalid("请导入或粘贴完整的单账号凭据 JSON，不接受文件路径或单独的令牌"))?;
    if !value.is_object() {
        return Err(errors::invalid("CodeArts 凭据必须是单个 JSON 对象"));
    }
    Ok(value)
}

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|text| !text.is_empty())
        .unwrap_or("")
        .to_string()
}

pub fn finite(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite())
}

pub fn invalidated() -> Error {
    Error::http(
        409,
        "CodeArts 账号、身份或凭据已改变，未复用此前结果；请重新查询",
    )
    .with_code("codearts_account_changed")
}
