//! 国际每日通用 Credits 的当日窗口：北京时间 10:00。不开窗不借昨天的活动记今天。
//! 窗口后一小时内十分钟重查一次；缺宿主 UMID 时不安排无意义的定时重试。
use super::{campaigns, util};
use agent2api_wasm_sdk::Value;

pub const GAP: i64 = 10 * 60_000;
const DAY: i64 = 86_400_000;
const GRACE: i64 = 60 * 60_000;

pub fn start(now: i64) -> i64 { (now + 8 * 3_600_000).div_euclid(DAY) * DAY + 2 * 3_600_000 }
pub fn current(row: &Value, now: i64) -> bool {
    let window = start(now);
    now >= window
        && campaigns::daily(row)
        && util::timestamp(&row["startAt"]).is_some_and(|at| at >= window && at <= now)
        && !util::timestamp(&row["endAt"]).is_some_and(|at| at <= now)
}
pub fn retry(now: i64) -> Option<i64> {
    let window = start(now);
    if now < window {
        return Some(window);
    }
    let next = now.saturating_add(GAP);
    (next <= window + GRACE).then_some(next)
}
