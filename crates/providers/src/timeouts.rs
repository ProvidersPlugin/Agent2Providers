//! 网关下发的四项超时（插件侧的只读快照）。
//!
//! ── 为什么插件里没有这份配置 ────────────────────────────────
//! 网关的 `config::timeout_settings()` 读的是它自己的配置文件（用户可以在设置
//! 页改「建连 / 等响应头 / 流空闲 / 响应体」四项）。插件进程**没有那份配置**：
//! 协议里没有下发超时设置的方法，`HostEnv` 也不带这四个值。
//!
//! ── 那这里给什么 ────────────────────────────────────────────
//! 与网关**默认值逐字相同**的常量（`DEFAULT_TIMEOUT_*_SECONDS`）。后果是：
//! 用户在设置页把「流空闲」从 300 秒改成 600 秒时，插件家的流仍按 300 秒判
//! 空闲超时。这是一个**已知的行为差**，不是疏漏 —— 要消除它需要给协议加一个
//! 「下发超时设置」的方法（或让 `HostEnv` 带上这四个值），那是一次契约变更。
//!
//! 已经在这里注明的原因是：让下一个改这块的人先看到「为什么是常量」，而不是
//! 以为漏读了配置。

/// 建连超时（与网关 `DEFAULT_TIMEOUT_CONNECT_SECONDS` 同值）
const CONNECT_SECONDS: i64 = 30;
/// 等响应头超时（与网关 `DEFAULT_TIMEOUT_HEADERS_SECONDS` 同值）
const HEADERS_SECONDS: i64 = 300;
/// 流式相邻数据之间的最大空闲（与网关 `DEFAULT_TIMEOUT_STREAM_IDLE_SECONDS` 同值）
const STREAM_IDLE_SECONDS: i64 = 300;
/// 非流式响应体读完的总预算（与网关 `DEFAULT_TIMEOUT_BODY_SECONDS` 同值）
const BODY_SECONDS: i64 = 300;

/// 超时快照（字段与网关 `TimeoutSettings` 同名，方便搬来的代码逐字复用）。
///
/// `headers_seconds` / `body_seconds` 在插件里没有读者（搬来的家目前只用
/// 建连与流空闲两项）—— 保留是为了让四个值成组出现，与网关那份对齐。
pub struct Timeouts {
    pub connect_seconds: i64,
    pub headers_seconds: i64,
    pub stream_idle_seconds: i64,
    pub body_seconds: i64,
}

impl Timeouts {
    fn ms(seconds: i64) -> u64 {
        // 负数 / 0 按最小值兜底（与网关同一防御：转 u64 前必须挡住）
        seconds.max(1) as u64 * 1000
    }

    pub fn connect_ms(&self) -> u64 {
        Self::ms(self.connect_seconds)
    }

    #[allow(dead_code)]
    pub fn headers_ms(&self) -> u64 {
        Self::ms(self.headers_seconds)
    }

    pub fn stream_idle_ms(&self) -> u64 {
        Self::ms(self.stream_idle_seconds)
    }

    #[allow(dead_code)]
    pub fn body_ms(&self) -> u64 {
        Self::ms(self.body_seconds)
    }
}

/// 当前超时（插件里恒为默认值，见模块头）。
pub fn current() -> Timeouts {
    Timeouts {
        connect_seconds: CONNECT_SECONDS,
        headers_seconds: HEADERS_SECONDS,
        stream_idle_seconds: STREAM_IDLE_SECONDS,
        body_seconds: BODY_SECONDS,
    }
}
