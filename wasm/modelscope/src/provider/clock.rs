//! 每日流水与冷却沿用北京零点；时间只由宿主提供，不读 WASI 或本机时区。

use agent2api_wasm_sdk::{self as sdk, Error, Result};
use chrono::{DateTime, FixedOffset, Offset};

pub fn offset() -> FixedOffset {
    FixedOffset::east_opt(8 * 3600).unwrap_or_else(|| chrono::Utc.fix())
}

pub fn today() -> Result<String> {
    DateTime::from_timestamp_millis(sdk::now())
        .map(|time| time.with_timezone(&offset()).format("%Y-%m-%d").to_string())
        .ok_or_else(|| Error::new("宿主时间无效，无法确认魔粒流水所属日期"))
}

pub fn next_day_boundary(now: i64) -> i64 {
    const DAY_MS: i64 = 86_400_000;
    const OFFSET_MS: i64 = 8 * 3600 * 1000;
    let shifted = now + OFFSET_MS;
    shifted.div_euclid(DAY_MS) * DAY_MS + DAY_MS - OFFSET_MS
}
