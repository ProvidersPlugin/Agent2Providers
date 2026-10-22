//! JS 语义工具（`Number()` / `parseTime` / `String(Math.floor(n))` 的等价物）。
//!
//! 从网关 `billing::request` 的工具段原样搬来 —— 这些函数服务的正是
//! 「把上游那套 JS 写法的计费响应翻译成前端契约」这件事，两处各写一份必然
//! 在时区、假值、`0` 与 `null` 的边界上分叉。
//!
//! ── 时区语义必须逐字对齐（实测踩过）────────────────────────
//! JS 的 `new Date(str)` 对**无时区**的格式分两类，差 8 小时（东八区实测）：
//!   `"2026-09-30"`（仅日期）      → **UTC** 零点（ECMAScript 规范规定）
//!   `"2026-09-30 23:59:59"`       → **本地时区**（实现相关的宽松解析）
//! 上游的 `CycleEndTime` / `CycleStartTime` 正是 `"YYYY-MM-DD HH:MM:SS"` 形态，
//! 按 UTC 解析会让 `refreshAt` 整体偏 8 小时。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::Value;

/// JS 真值判定（`Boolean(x)`）：null/false/0/"" 为假，空数组/空对象为真
pub(super) fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// JS `Number(x) || 0`：非数字/NaN/0/缺失都得到 0
pub(super) fn number_or_zero(value: Option<&Value>) -> f64 {
    let number = match value {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    match number {
        Some(number) if number.is_finite() => number,
        _ => 0.0,
    }
}

/// JS `toInt(value)`：`Math.floor(Number(value))`，NaN → null
pub(super) fn to_int(value: &Value) -> Option<i64> {
    let number = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    match number {
        Some(number) if number.is_finite() => Some(number.floor() as i64),
        _ => None,
    }
}

/// 解析时间字段为毫秒时间戳（对照 Node 版 `parseTime` 的**原始返回值**）。
///
/// 语义逐条对齐：
///   - 缺失 / null / 空串 → 0（不是 None）
///   - 纯数字字符串（`/^\d+$/`）→ 数字本身（上游有时把时间戳给成字符串）
///   - 其余交给 `new Date(x).getTime()`；解析不出 → 0
///
/// ── 时区语义必须逐字对齐（实测踩过）────────────────────────
/// JS 的 `new Date(str)` 对**无时区**的格式分两类，差 8 小时（东八区实测）：
///   `"2026-09-30"`（仅日期）      → **UTC** 零点（ECMAScript 规范规定）
///   `"2026-09-30 23:59:59"`       → **本地时区**（实现相关的宽松解析）
///   `"2026-09-30T23:59:59"`       → **本地时区**
/// 上游的 `CycleEndTime` / `CycleStartTime` 正是 `"YYYY-MM-DD HH:MM:SS"` 形态，
/// 若在这里按 UTC 解析，`refreshAt` 会整体偏 8 小时，前端显示的「额度刷新时间」
/// 就与 Node 版对不上。因此这里对「无时区的日期时间」用 `Local` 解析，
/// 「仅日期」用 UTC。
///
/// 返回 i64 而不是 Option：调用方需要的正是「0 为失败哨兵」这件事，
/// 好让 `|| 0` / `|| null` 这些 JS 假值判断在 Rust 侧一目了然。
pub(super) fn parse_time(value: Option<&Value>) -> i64 {
    time_or_null(value).unwrap_or(0)
}

/// `parseTime(x) || null` 的等价物：解析不出（得到 0）时给 None。
///
/// 单独一个函数是因为 Node 里这两种写法混用：`parseTime(x) || parseTime(y)`
/// 会跳过第一个解不出的，而 `parseTime(x) || 0` 会退化成 0。把「0 = 失败」
/// 显式化成 None 之后，Rust 侧的 `or_else` / `unwrap_or` 就能一一对应上去。
pub(super) fn time_or_null(value: Option<&Value>) -> Option<i64> {
    let text = match value {
        None | Some(Value::Null) => return None,
        Some(Value::Number(number)) => {
            let parsed = number.as_f64()? as i64;
            return if parsed == 0 { None } else { Some(parsed) };
        }
        Some(Value::String(text)) => text.trim().to_string(),
        Some(_) => return None,
    };
    if text.is_empty() {
        return None;
    }
    // `/^\d+$/` 的等价判断：全数字按时间戳读
    let parsed = if text.chars().all(|ch| ch.is_ascii_digit()) {
        text.parse::<i64>().ok()
    } else {
        parse_date_text(&text)
    };
    // 0 是「解不出」的哨兵值（与 parseTime 返回 0 等价）
    parsed.filter(|value| *value != 0)
}

/// 日期文本 → 毫秒时间戳，时区语义与 JS 的 `new Date(str)` 对齐（见 `parse_time`）。
///
/// 只覆盖上游真会给到的几种形态；认不出的一律 None（JS 会得到 NaN → 0）。
fn parse_date_text(text: &str) -> Option<i64> {
    use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone, Utc};

    // 带时区偏移：交给 chrono 直接按绝对时间解析（与 JS 一致）
    if let Ok(value) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(value.timestamp_millis());
    }
    // 仅日期：JS 规范规定按 **UTC** 解读（不是本地！）
    if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return date
            .and_hms_opt(0, 0, 0)
            .map(|value| value.and_utc().timestamp_millis());
    }
    // 无时区的日期时间：按**本地时区**解读（"YYYY-MM-DD HH:MM:SS" 与 ISO 的 T 形态）
    let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S"))
        .ok()?;
    // 本地时刻可能因夏令时不存在（春季跳变）或重复（秋季回落）：
    // 不存在时退化为 UTC 解读（宁可偏一点，也不要整条链路上报错）
    Local
        .from_local_datetime(&naive)
        .single()
        .map(|value| value.timestamp_millis())
        .or_else(|| Some(Utc.from_utc_datetime(&naive).timestamp_millis()))
}

/// 时间戳 → JSON（None → null；整数形态）
pub(super) fn timestamp_json(value: Option<i64>) -> Value {
    value.map(Value::from).unwrap_or(Value::Null)
}

/// JS `String(Math.floor(n))` 的近似：整数输出整数形态，
/// 浮点（理论上是小数额度）保留一位小数 —— 上游额度是整数，
/// 这里只是不让 `0.0` 这种形态漏到界面上。
pub(super) fn js_int_string(value: f64) -> String {
    if !value.is_finite() {
        return "0".to_string();
    }
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        let text = format!("{value}");
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}
