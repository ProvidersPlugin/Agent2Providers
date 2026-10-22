//! 上游限额恢复时间的解析（`YYYY-MM-DD HH:MM:SS UTC+8` → 毫秒时间戳）。
//!
//! 从网关 `server::errors` 原样搬来：workbuddy 的 429（码 6004）把恢复时间写在
//! **文案里**（`您的使用量已超出频率限制，将在 2026-09-11 19:43:46 UTC+8 重置…`），
//! 不解析就只能落「10 分钟兜底」冷却。
//!
//! ── 为什么是「按字节扫描」而不是正则/切片 ────────────────────
//! 上游的 msg 是**中文**：上一版用 `&text[start..start+19]` 滑动窗口，窗口起点
//! 一旦落在汉字中间就在非字符边界上切片 —— release 是 `panic=abort`，会带走
//! 整个进程。实测这条路径在真实 429 响应上 100% 触发。全程按 `&[u8]` 判定之后，
//! 只有「全是 ASCII 数字与 `-`」的位置才继续，后续任何切片都天然落在字符边界上。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use chrono::TimeZone;

/// 解析上游限额恢复时间：`YYYY-MM-DD HH:MM:SS UTC+8` → 毫秒时间戳。
///
/// 对应 Node 版 parseQuotaResetAt（workbuddy-upstream-client.mjs 82-87 行）：
/// ```js
/// /(\d{4})-(\d{2})-(\d{2})\s+(\d{2}):(\d{2}):(\d{2})\s*UTC\+8/
/// ```
/// 解析不出时返回 0（Node 版同样返回 0，调用方据此决定要不要带 reset_at 字段），
/// 而不是报错 —— 这条路径在处理上游错误，不能再制造新错误。
///
/// ── 为什么改写成「按字节扫描 + 数值解析」（切片 4 发现并修复）────
/// 上一版是「滑动窗口 + `&text[start..start+19]` 切片」，在**中文错误文案**上
/// 会直接 panic（release 是 panic=abort，会带走整个桌面应用）：
/// 上游 429 的 msg 是「您的使用量已超出频率限制，将在 2026-09-11 19:43:46
/// UTC+8 重置…」，窗口起点一旦落在汉字中间（如「您」的第 2、3 字节），
/// 切片就在非字符边界上，`is_char_boundary` 的检查写在切片**之后**已经来不及。
/// 实测这条路径在真实 429 响应上 100% 触发。
///
/// 现在全程按 `&[u8]` 判定：只有当某个位置起确实是 `dddd-dd-dd` 形态
/// （全是 ASCII 数字与 `-`）才继续，而 ASCII 字节不可能是多字节字符的
/// 续字节，所以后续任何切片都天然落在字符边界上。顺带把 Node 正则里
/// 的 `\s+`（日期与时间之间至少一个空白）与 `\s*UTC+8` 后缀也补齐了 ——
/// 上一版只认「一个空格 + 19 字符定长窗口」且不校验 UTC+8。
pub fn parse_quota_reset_at(text: &str) -> i64 {
    parse_quota_reset_at_at(text, crate::logging::now_ms())
}

/// [`parse_quota_reset_at`] 的**可注入「现在」**版本。
///
/// 只有「当日 / 今日 HH:MM」那一形需要知道今天是哪天（见 `scan_same_day_window`），
/// 所以把 now 提出来当入参 —— 线上入口仍是不带 now 的那个，测试钉死日期用这个。
pub fn parse_quota_reset_at_at(text: &str, now_ms: i64) -> i64 {
    let bytes = text.as_bytes();
    // 空白判定：`\s` 在 Node 正则里含空格/制表/换行（字节层面判 ASCII 空白即可）
    let space = |index: usize| -> bool {
        bytes
            .get(index)
            .map(|byte| byte.is_ascii_whitespace())
            .unwrap_or(false)
    };
    /// 取 `width` 位十进制数字（不足或非数字返回 None）
    fn number(bytes: &[u8], start: usize, width: usize) -> Option<u32> {
        let mut value = 0u32;
        for offset in 0..width {
            let byte = bytes.get(start + offset)?;
            if !byte.is_ascii_digit() {
                return None;
            }
            value = value * 10 + (byte - b'0') as u32;
        }
        Some(value)
    }

    let mut index = 0usize;
    while index < bytes.len() {
        // ① dddd-dd-dd
        let date = (
            number(bytes, index, 4),
            bytes.get(index + 4),
            number(bytes, index + 5, 2),
            bytes.get(index + 7),
            number(bytes, index + 8, 2),
        );
        if let (Some(year), Some(b'-'), Some(month), Some(b'-'), Some(day)) = date {
            // ② \s+ 分隔
            let mut cursor = index + 10;
            let space_start = cursor;
            while space(cursor) {
                cursor += 1;
            }
            if cursor > space_start {
                // ③ dd:dd:dd
                let time = (
                    number(bytes, cursor, 2),
                    bytes.get(cursor + 2),
                    number(bytes, cursor + 3, 2),
                    bytes.get(cursor + 5),
                    number(bytes, cursor + 6, 2),
                );
                if let (Some(hour), Some(b':'), Some(minute), Some(b':'), Some(second)) = time {
                    // ④ \s*UTC+8 —— 必须命中，否则不是限额恢复时间（与 Node 正则一致）
                    let mut suffix = cursor + 8;
                    while space(suffix) {
                        suffix += 1;
                    }
                    if bytes[suffix..].starts_with(b"UTC+8") {
                        if let Some(millis) = to_utc_plus_8_millis(year, month, day, hour, minute, second) {
                            return millis;
                        }
                    }
                }
            }
        }
        index += 1;
    }
    // ② 「当日 / 今日 HH:MM」—— 另一种常见写法：没有日期、没有秒、也没有
    //    `UTC+8` 后缀，所以上面的扫描认不出来。AtomCode 的配额文案就是这个形态：
    //    「…当前滚动窗口约在当日 18:18 结束并重置额度，请稍后重试。」
    //    （2026-10-18 在 dev 上逐字抓过）。⚠️ 改这一形之前，这类文案解析出来是 0，
    //    于是冷却落到「10 分钟兜底」而不是真正的窗口重置点。
    scan_same_day_window(text, now_ms).unwrap_or(0)
}

/// 「当日 / 今日 HH:MM」→ 北京时间**当天**那个时刻的毫秒时间戳。
///
/// 判据（比完整时刻形宽松，因为它本来就没给日期与秒）：
///   · 关键词 `当日` / `今日` 之后的 **16 个字符**内出现 `H[H]:MM`；
///   · 越界（`25:00` 之类）与「分钟后面还接数字」都不认；
///   · 算出来**已经过去** ⇒ `None`（这个窗口早就重置了。返回一个过去的时刻
///     等于让调用方写一条立刻过期的冷却，比不解析更坏）。
fn scan_same_day_window(text: &str, now_ms: i64) -> Option<i64> {
    let date = crate::beijing::date_time_of_ms(now_ms)?.date_naive();
    for keyword in ["当日", "今日"] {
        let Some(at) = text.find(keyword) else {
            continue;
        };
        // 按**字符**取窗口：`&rest[..16]` 落在汉字中间会 panic
        // （release profile 是 panic=abort，这类切片错误会带走整个进程）
        let rest: String = text[at + keyword.len()..].chars().take(16).collect();
        let Some((hour, minute)) = first_clock(&rest) else {
            continue;
        };
        let Some(naive) = date.and_hms_opt(hour, minute, 0) else {
            continue;
        };
        let instant = crate::beijing::offset()
            .from_local_datetime(&naive)
            .single()?;
        let millis = instant.timestamp_millis();
        if millis > now_ms {
            return Some(millis);
        }
    }
    None
}

/// 文本里第一处 `H[H]:MM`（小时 1–2 位、分钟必须 2 位，且分钟之后不再接数字）。
fn first_clock(text: &str) -> Option<(u32, u32)> {
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let mut cursor = index;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
        let hour_digits = cursor - index;
        // 全是 ASCII 数字 ⇒ 按字节切片天然落在字符边界上
        let hour = text[index..cursor].parse::<u32>().ok();
        let minute = (bytes.get(cursor) == Some(&b':')
            && bytes.get(cursor + 1).is_some_and(u8::is_ascii_digit)
            && bytes.get(cursor + 2).is_some_and(u8::is_ascii_digit))
        .then(|| u32::from(bytes[cursor + 1] - b'0') * 10 + u32::from(bytes[cursor + 2] - b'0'));
        let trailing_digit = bytes.get(cursor + 3).is_some_and(u8::is_ascii_digit);
        if let (Some(hour), Some(minute)) = (hour, minute) {
            if (1..=2).contains(&hour_digits) && !trailing_digit && hour < 24 && minute < 60 {
                return Some((hour, minute));
            }
        }
        index = cursor;
    }
    None
}

/// `YYYY-MM-DD HH:MM:SS`（UTC+8）→ 毫秒时间戳；字段非法时 None。
///
/// 不引入 regex：手写扫描比正则更容易控制「失败即 0」的语义。
fn to_utc_plus_8_millis(
    year: u32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Option<i64> {
    // UTC+8 固定偏移：上游文案里就是这么标的，不做时区探测
    let offset = chrono::FixedOffset::east_opt(8 * 3600)?;
    let naive = chrono::NaiveDate::from_ymd_opt(year as i32, month, day)?
        .and_hms_opt(hour, minute, second)?;
    Some(offset.from_local_datetime(&naive).single()?.timestamp_millis())
}
