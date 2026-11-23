//! 本包的设置仅约束首帧前排队，不开放域名、机器指纹、风控票据或其它 provider 配置。
use super::{endpoints, machine};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub fn queue_waits() -> Result<u64> {
    let value = sdk::settings::get("queueMaxWaits")?;
    if value.is_null() {
        return Ok(2);
    }
    value
        .as_u64()
        .filter(|value| *value <= 10)
        .ok_or_else(|| Error::http(400, "千问办公排队次数应为 0 到 10"))
}
pub fn queue_budget() -> Result<i64> {
    let value = sdk::settings::get("queueBudgetSeconds")?;
    if value.is_null() {
        return Ok(60_000);
    }
    value
        .as_i64()
        .filter(|value| (0..=300).contains(value))
        .map(|value| value * 1000)
        .ok_or_else(|| Error::http(400, "千问办公排队时长应为 0 到 300 秒"))
}
pub fn schema() -> Value {
    json!({ "version": 1, "title": "千问办公 国际版", "runtime_overrides": false,
        "description": format!("{} {}", endpoints::EVIDENCE_NOTE, machine::UUID_NOTE),
        "fields": [
            { "key": "queueMaxWaits", "label": "最多排队等待次数", "type": "number", "default": 2, "minimum": 0, "maximum": 10 },
            { "key": "queueBudgetSeconds", "label": "本家排队等待上限（秒）", "type": "number", "default": 60, "minimum": 0, "maximum": 300 }
        ] })
}
