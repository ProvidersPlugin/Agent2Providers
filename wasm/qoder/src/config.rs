//! Qoder 独立设置只限制首帧前排队等待；系统请求预算最终由宿主统一约束。
//! 不接受自定义域名、版本指纹、机器 OS 或 UMID 设置。
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub fn queue_waits() -> Result<u64> {
    let value = sdk::settings::get("queueMaxWaits")?;
    if value.is_null() { return Ok(2); }
    value.as_u64().filter(|value| *value <= 10).ok_or_else(|| Error::http(400, "Qoder 排队次数应为 0 到 10"))
}
pub fn queue_budget() -> Result<i64> {
    let value = sdk::settings::get("queueBudgetSeconds")?;
    if value.is_null() { return Ok(30_000); }
    value.as_i64().filter(|value| (0..=300).contains(value)).map(|value| value * 1000)
        .ok_or_else(|| Error::http(400, "Qoder 排队时长应为 0 到 300 秒"))
}
pub fn schema() -> Value {
    json!({ "version": 1, "title": "Qoder 中国版", "runtime_overrides": false,
        "description": "控制首帧前遇到 10605 排队时的本家轮询等待（设为 0 则立即释放排队占位并交由主程序换号）。总体请求超时由主程序控制，不读取国际版或千问办公设置。",
        "fields": [
            { "key": "queueMaxWaits", "label": "最多排队等待次数", "type": "number", "default": 2, "minimum": 0, "maximum": 10 },
            { "key": "queueBudgetSeconds", "label": "本家排队等待上限（秒）", "type": "number", "default": 30, "minimum": 0, "maximum": 300 }
        ] })
}
