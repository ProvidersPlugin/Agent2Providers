//! 有界纯解析；时钟和随机数只能来自现有宿主能力，不读系统环境。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use base64::Engine;

use crate::{endpoints, errors};

pub fn text(value: &Value, fields: &[&str]) -> String {
    fields.iter().find_map(|field| scalar(value.get(*field)).filter(|text| !text.trim().is_empty()))
        .unwrap_or_default().trim().to_string()
}

pub fn scalar(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

pub fn safe_text(text: &str, maximum: usize) -> String {
    text.trim().chars().filter(|ch| !ch.is_control()).take(maximum).collect()
}

pub fn json_object(text: &str) -> Result<Value> {
    if text.len() > endpoints::MAX_INPUT_BYTES {
        return Err(errors::invalid("CatPaw 凭据文本超过 128 KiB 上限"));
    }
    let value: Value = serde_json::from_str(text.trim())
        .map_err(|_| errors::invalid("CatPaw 凭据不是有效 JSON，请提供单账号对象或 auth.json 内容"))?;
    if !value.is_object() { return Err(errors::invalid("CatPaw 凭据 JSON 必须是单账号对象")); }
    Ok(value)
}

pub fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }.filter(|number| number.is_finite())
}

pub fn integer(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

pub fn flag(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::Number(number) if number.as_i64() == Some(0) => Some(false),
        Value::Number(number) if number.as_i64() == Some(1) => Some(true),
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

pub fn now() -> Result<i64> {
    sdk::host("clock.now", Value::Null).map_err(errors::host)?.as_i64()
        .filter(|time| *time > 0)
        .ok_or_else(|| Error::http(503, "主程序未提供有效时间，未把目录记为已刷新").with_code("catpaw_clock_unavailable"))
}

pub fn trace_id() -> Result<String> {
    let value = sdk::host("random.bytes", json!({ "length": 16 })).map_err(errors::host)?;
    let mut bytes = base64::engine::general_purpose::STANDARD.decode(value.as_str().unwrap_or(""))
        .map_err(|_| Error::http(503, "主程序随机数格式无效"))?;
    if bytes.len() != 16 { return Err(Error::http(503, "主程序随机数长度无效")); }
    // M-TRACEID 是 UUID v4 去连字符；格式与原生 request_headers 一致。
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
