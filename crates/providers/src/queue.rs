//! 排队等待的插件侧等价物（网关 `config::queue_settings` 的只读快照）。
//!
//! ── 为什么插件里没有这份配置 ────────────────────────────────
//! 网关的排队设置（`queueMaxWaits` / `queueWaitSeconds`，设置页「通用 →
//! 排队等待」）住在它自己的 `config.json` 里。插件进程**没有那份配置**：
//! 协议没有下发它的方法，`HostEnv` 也不带。
//!
//! ── 所以这里给默认值（与网关 `QueueSettings::default()` 逐字相同）─────
//! 后果：用户把「最多等 5 次」改成别的值时，插件家的排队仍按默认的「最多等
//! 2 次、单次跟随上游建议」执行。这是**已知的行为差**，不是疏漏 —— 要消除它
//! 需要给协议加一个「下发排队设置」的方法。
//!
//! ── 哪些家受影响 ────────────────────────────────────────────
//! 目前只有 Qoder（它的免费模型走上游排队制）。

/// 最多等待次数（与网关 `DEFAULT_QUEUE_MAX_WAITS` 同值）
const MAX_WAITS: i64 = 2;
/// 单次等待秒数（与网关 `DEFAULT_QUEUE_WAIT_SECONDS` 同值；`0` = 跟随上游建议）
const WAIT_SECONDS: i64 = 0;
/// 次数下限 / 上限（与网关同值）
const MIN_MAX_WAITS: i64 = 0;
const MAX_MAX_WAITS: i64 = 10;
/// 单次时长下限 / 上限（与网关同值）
const MIN_WAIT_SECONDS: i64 = 0;
const MAX_WAIT_SECONDS: i64 = 120;

/// 排队设置快照（字段与网关 `QueueSettings` 同名）。
pub struct QueueSettings {
    pub max_waits: i64,
    pub wait_seconds: i64,
}

impl QueueSettings {
    /// 等待次数预算。
    pub fn wait_budget(&self) -> usize {
        self.max_waits.clamp(MIN_MAX_WAITS, MAX_MAX_WAITS) as usize
    }

    /// 强制单次等待时长（毫秒）；`None` = 跟随上游建议。
    pub fn forced_wait_ms(&self) -> Option<u64> {
        let seconds = self.wait_seconds.clamp(MIN_WAIT_SECONDS, MAX_WAIT_SECONDS);
        if seconds > 0 {
            Some(seconds as u64 * 1000)
        } else {
            None
        }
    }
}

impl Default for QueueSettings {
    fn default() -> Self {
        Self {
            max_waits: MAX_WAITS,
            wait_seconds: WAIT_SECONDS,
        }
    }
}

/// 当前排队设置（插件里恒为默认值，见模块头）。
pub fn queue_settings() -> QueueSettings {
    QueueSettings::default()
}
