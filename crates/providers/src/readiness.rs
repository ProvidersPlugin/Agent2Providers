//! **本地就绪闸门**：一条通道在「还没出本机就注定发不出去」时，让插件自己记住。
//!
//! ── 与网关那份的关系（迁移后改了什么）────────────────────────
//! 网关的 `core::providers::readiness` 有**三个**消费方：适配器 `hold`（判别）、
//! 这个模块自己 `release`（令牌入库时撤）、以及选路口 `rotate` / `provider_loop`
//! 读 [`gated`]（绕开）。
//!
//! 插件里只剩前两个：**选路在网关侧**，插件读不到它的候选池，也没有办法让
//! 网关「绕开这一家」。所以这里保留 `hold` / `release` / `gated` / `snapshot`，
//! 但 `gated` 在插件里只服务排障（`captcha::stats` 里的 `localGates` 那类展示），
//! **不再参与任何选路**。
//!
//! ── 那还存在什么意义 ────────────────────────────────────────
//! `captcha` 的 `hold` 仍然要记一笔：构造期失败是**逐请求**发生的，
//! 记下来才能让「这一家在本机发不出去」这件事在插件自己的日志与状态里看得见
//! （而不是每条请求都打一行同样的失败）。`release` 也照旧 —— 令牌入库时
//! 撤掉它，日志里那一行「已撤」就是判据恢复的落点。
//!
//! ── 闸门为什么带寿命 ────────────────────────────────────────
//! 与网关同一理由：只能靠入库解除的话，「铸造链换了出口」会让这一家被永久
//! 误挡。到期自动放行让下一次构造重新探一次本地条件，代价有界。
//!
//! ── 锁中毒怎么办 ──────────────────────────────────────────
//! 退化成「无闸门」（放行）。闸门是优化，不是安全边界。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::logging;

/// 「这一家在**本机**就发不出去」的专用状态码（与网关同值）。
///
/// ⚠️ 插件发出的 `build_chat` 错误过线后一律成为网关的 502（协议的
/// `Result<_, String>` 不带状态码），所以这个数字在插件里**只是日志与排障的
/// 标记**，不再有编排语义。保留同值是为了与网关那份对照着读日志。
pub const LOCAL_PRECONDITION_STATUS: u16 = 556;

/// 闸门寿命（毫秒），与 `captcha::TOKEN_TTL_MS` 同量级。
pub const GATE_TTL_MS: i64 = 120_000;

/// 一道闸门的登记项。
#[derive(Clone, Debug)]
struct Gate {
    /// 给人读的那句话（面板与终端日志用；**不参与任何判定**）。
    reason: String,
    /// 闸门**成立**的时刻（首次 `hold`）。已成立时再 hold 不刷新。
    held_at: i64,
}

#[derive(Default)]
struct Registry {
    gates: HashMap<String, Gate>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

/// 登记一道闸门。返回 `true` = 本次让闸门从放行变成挡住。
///
/// **已成立时不刷新 `held_at`**：续期的话，繁忙的插件会每隔几秒把时间戳顶新
/// 一次，闸门永远到不了期 —— 那等于把自愈能力抹掉。
pub fn hold(provider_id: &str, reason: &str) -> bool {
    hold_at(provider_id, reason, logging::now_ms())
}

/// [`hold`] 的可注入时钟版本（保留给将来的用例与排障）。
pub fn hold_at(provider_id: &str, reason: &str, now: i64) -> bool {
    let provider_id = provider_id.trim();
    if provider_id.is_empty() {
        return false;
    }
    let Ok(mut table) = registry().lock() else {
        return false;
    };
    match table.gates.get_mut(provider_id) {
        Some(existing) => {
            existing.reason = reason.to_string();
            false
        }
        None => {
            table.gates.insert(
                provider_id.to_string(),
                Gate {
                    reason: reason.to_string(),
                    held_at: now,
                },
            );
            true
        }
    }
}

/// 撤掉闸门（令牌入库、或本地条件确认恢复时调用）。返回 `true` = 确实撤掉了。
pub fn release(provider_id: &str) -> bool {
    let provider_id = provider_id.trim();
    if provider_id.is_empty() {
        return false;
    }
    let Ok(mut table) = registry().lock() else {
        return false;
    };
    let Some(gate) = table.gates.remove(provider_id) else {
        return false;
    };
    logging::log(
        "[Routing]",
        &format!(
            "✅ {provider_id} 的本地闸门已撤（挡了 {}s，原因：{}）",
            ((logging::now_ms() - gate.held_at) / 1000).max(0),
            gate.reason,
        ),
    );
    true
}

/// 这一家当前是否被闸门挡住（顺带清掉已过期的登记项）。
#[allow(dead_code)]
// 插件里没有选路可绕（选路在网关侧），`gated` 只服务排障展示。
// 保留：它与 `snapshot` 一起回答「现在挡着谁」，而 `snapshot` 也被
// `captcha::stats` 的同源逻辑需要。
pub fn gated(provider_id: &str) -> bool {
    let provider_id = provider_id.trim();
    if provider_id.is_empty() {
        return false;
    }
    let now = logging::now_ms();
    let Ok(mut table) = registry().lock() else {
        return false;
    };
    match table.gates.get(provider_id) {
        Some(gate) if now - gate.held_at < GATE_TTL_MS => true,
        Some(_) => {
            table.gates.remove(provider_id);
            false
        }
        None => false,
    }
}

/// 测试用：串行化所有碰这张全局表的用例。
///
/// 闸门表是**进程级**状态，同一份 `cargo test` 二进制里所有用例共享它 ——
/// 并发的用例能把对方的前提改掉，那种红法与绿法都不可信。与网关侧
/// `core::providers::readiness::lock_for_tests` 同一手法（插件里没有
/// `clear_for_tests` 的调用方，用例自己按需 hold / release）。
#[cfg(test)]
pub fn lock_for_tests() -> std::sync::MutexGuard<'static, ()> {
    static SLOT: Mutex<()> = Mutex::new(());
    // 中毒继续用同一把锁：某个用例 panic 过已经被执行器记成失败，这里要做的
    // 只是让后面的用例照样跑完，而不是把「锁坏了」再报一次
    SLOT.lock().unwrap_or_else(|error| error.into_inner())
}

/// 当前所有**未过期**的闸门（`[{provider, reason, ageMs}]`）。
#[allow(dead_code)]
// 与 `gated` 同理：没有插件侧调用方，是排障视图的入口。
pub fn snapshot() -> Vec<Value> {
    let now = logging::now_ms();
    let Ok(mut table) = registry().lock() else {
        return Vec::new();
    };
    table
        .gates
        .retain(|_, gate| now - gate.held_at < GATE_TTL_MS);
    let mut rows: Vec<Value> = table
        .gates
        .iter()
        .map(|(provider, gate)| {
            json!({
                "provider": provider,
                "reason": gate.reason,
                "ageMs": (now - gate.held_at).max(0),
            })
        })
        .collect();
    // 稳定顺序：HashMap 的迭代序不定，排一下让日志与界面可对照
    rows.sort_by(|left, right| {
        left.get("provider")
            .and_then(Value::as_str)
            .cmp(&right.get("provider").and_then(Value::as_str))
    });
    rows
}
