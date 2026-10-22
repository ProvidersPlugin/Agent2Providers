//! 时间解析小工具（RFC 3339 → 毫秒时间戳）。
//!
//! ── 为什么不引 chrono 的 `DateTime::parse_from_rfc3339` ──────
//! 插件只需要「上游给的 `expires_at` 字符串 → 毫秒」这一件事，而 chrono 的
//! 解析器要处理时区偏移、闰秒、分数秒的各种写法。上游（华为云 STS）给的是
//! 固定形态 `YYYY-MM-DDTHH:MM:SSZ`，手写解析比引一条新链更可控，也与网关
//! `officeace::oauth` 那份实现逐字相同（迁移 parity 靠它核对）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 本 crate 编译期禁止 unwrap/expect/panic（见 `main.rs` 的 deny）。

/// 解析 `YYYY-MM-DDTHH:MM:SS[.fff]Z` 形态的时间戳，返回毫秒。
///
/// 非该形态（含带时区偏移的写法）一律返回 `None` —— 调用方按「到期时刻未知」
/// 处理（各家都有各自的兜底值），而不是拿一个猜出来的时刻当真。
pub fn parse_rfc3339_ms(input: &str) -> Option<i64> {
    let text = input.trim().trim_end_matches('Z');
    let (date, time) = text.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let second: i64 = time_parts
        .next()
        .and_then(|value| value.split('.').next())
        .unwrap_or("0")
        .parse()
        .ok()?;
    days_from_civil(year, month, day)
        .map(|days| (days * 86_400 + hour * 3600 + minute * 60 + second) * 1000)
}

/// Howard Hinnant 的 `days_from_civil`（年月日 → epoch 起的天数）。
///
/// 算法出处：<https://howardhinnant.github.io/date_algorithms.html>。不处理
/// 1582 年之前的历法切换（上游给的都是当代时间），也不做闰秒修正。
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let adjusted_year = if month <= 2 { year - 1 } else { year };
    let era = if adjusted_year >= 0 { adjusted_year } else { adjusted_year - 399 } / 400;
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}
