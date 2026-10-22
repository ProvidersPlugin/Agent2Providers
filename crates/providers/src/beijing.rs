//! 北京时间（固定 UTC+8）—— 签到链路对「今天」的唯一定义。
//!
//! ── 为什么插件里也要有这一层 ────────────────────────────────
//! 从网关搬来的 `raccoon/balance.rs` 用 `beijing::today_key()` 判「今日入账」。
//! 那套判据的依据与网关侧逐字相同：各家额度都在北京时间零点前后重置，
//! 而机器时区可能是任意值 —— 跟着机器时区走会让「今天」的边界整体漂移
//! （网关侧的原话见 `core::beijing` 的模块头，issue #138）。
//!
//! ── 为什么是固定偏移而不是时区库 ─────────────────────────────
//! 中国标准时间没有夏令时，`Asia/Shanghai` 在 1970 年之后恒为 +08:00 ——
//! 一个固定偏移就够，且不依赖宿主机的 tzdata。

// 这一层是给签到期分日用的共用工具，目前只有 `today_key` 有消费方
// （小浣熊搬进来之后其余几个也会被用到）。
#![allow(dead_code)]

use chrono::{DateTime, FixedOffset, Offset};

/// UTC+8 的固定偏移（秒）。
pub const OFFSET_SECONDS: i32 = 8 * 3600;

/// 固定 UTC+8 时区对象。
pub fn offset() -> FixedOffset {
    // 8 小时恒在合法范围内；兜底给 UTC 偏移是为了守住「插件零 panic」的约定。
    FixedOffset::east_opt(OFFSET_SECONDS).unwrap_or_else(|| chrono::Utc.fix())
}

/// 毫秒时间戳 → 北京时间时刻；越界的非法时间戳给 None。
pub fn date_time_of_ms(ms: i64) -> Option<DateTime<FixedOffset>> {
    DateTime::from_timestamp_millis(ms).map(|utc| utc.with_timezone(&offset()))
}

/// 北京时间当前时刻。
pub fn now() -> DateTime<FixedOffset> {
    date_time_of_ms(crate::http::now_ms())
        .unwrap_or_else(|| chrono::Utc::now().with_timezone(&offset()))
}

/// 北京日期键 `YYYY-MM-DD`（定长，字典序即时间序）。
pub fn date_key(now: DateTime<FixedOffset>) -> String {
    now.format("%Y-%m-%d").to_string()
}

/// 毫秒时间戳落在北京时间的哪一天（`YYYY-MM-DD`）。
pub fn date_key_of_ms(ms: i64) -> Option<String> {
    date_time_of_ms(ms).map(date_key)
}

/// 北京时间的今天（`YYYY-MM-DD`）。
pub fn today_key() -> String {
    date_key(now())
}
