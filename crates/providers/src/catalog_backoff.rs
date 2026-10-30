//! 目录**就地补拉**的失败冷却：按 `(provider, account)` 记一条短 TTL 的闸。
//!
//! ── 解决什么问题 ────────────────────────────────────────────
//! 「内存没有 → 台账没有 → 就地真拉一次」这条路上，失败**会重复**：上游目录
//! 接口故障期间，转发路径上每个请求都会各自再拉一遍（那条路上通常还没有会话闸
//! —— 会话闸在目录校验之后），一次抖动就这样被放大成一次雪崩。所以第一次失败
//! 记一条 30 秒冷却，期内直接按「还没拉取成功」回报，不再打上游。
//!
//! ── 三条口径（与 codearts 的 `size_gate` 同源）────────────────
//!   · **不持久**：TTL 到期自动放行 —— 上游恢复后不需要任何人工动作；
//!   · **只有失败才写**：成功不进冷却（成功之后内存里就有清单了，这条路不再走）；
//!   · **键是 `(provider, account)`**：目录按账号拉（CodeArts 的清单本身就是按
//!     账号算出来的），一个账号的失败不该拦住另一个账号。
//!
//! ── 与各家刷新链路的关系 ────────────────────────────────────
//! 这道闸只管**转发路径的兜底补拉**，不碰各家自己的刷新 TTL 与网关的刷新排期：
//! 那些是"什么时候该刷"，这里是"刚刚试过、别再连着试"。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 冷却时长。够挡住一次抖动里成串的重试，又不至于把一次偶发失败记成"这家坏了"。
const COOLING_MS: i64 = 30_000;

/// 上限：键是 provider×account，正常只有个位数；给一个上限防脏数据撑大内存。
const MAX_ENTRIES: usize = 256;

/// 一条冷却（还有多久、上次为什么失败）。
pub struct Cooling {
    pub reason: String,
    pub ms_left: i64,
}

fn gates() -> &'static Mutex<HashMap<String, (String, i64)>> {
    static GATES: OnceLock<Mutex<HashMap<String, (String, i64)>>> = OnceLock::new();
    GATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(provider: &str, account: &str) -> String {
    format!("{provider}\u{0}{account}")
}

/// 此刻是否在冷却中（已过期的那条顺手清掉，不留垃圾）。
pub fn cooling(provider: &str, account: &str, at_ms: i64) -> Option<Cooling> {
    let mut guard = match gates().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let key = key(provider, account);
    let (reason, expires_at) = guard.get(&key)?.clone();
    if expires_at <= at_ms {
        guard.remove(&key);
        return None;
    }
    Some(Cooling { reason, ms_left: expires_at - at_ms })
}

/// 记一次失败（TTL 从本次起算；同一键重复失败会刷新剩余时间与原因）。
pub fn hold(provider: &str, account: &str, reason: &str, at_ms: i64) {
    let mut guard = match gates().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.retain(|_, (_, expires_at)| *expires_at > at_ms);
    if guard.len() >= MAX_ENTRIES {
        guard.clear();
    }
    guard.insert(key(provider, account), (reason.to_string(), at_ms + COOLING_MS));
}

/// 补拉成功 / 从台账恢复成功后清掉冷却。
pub fn clear(provider: &str, account: &str) {
    if let Ok(mut guard) = gates().lock() {
        guard.remove(&key(provider, account));
    }
}

/// 冷却还剩几秒（文案用；至少 1 秒，免得显示"0 秒后重试"）。
pub fn seconds_left(ms_left: i64) -> i64 {
    (ms_left / 1000).max(1)
}
