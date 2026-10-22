//! 日志：插件侧的两个入口，转发到 SDK 的回传通道。
//!
//! ── 为什么要一层薄封装而不是各家直接调 SDK 宏 ──────────────────
//! 从网关搬来的代码里到处是 `logging::log("[Loomy]", "…")` 与
//! `logging::verbose(…)`。搬家时如果每处都改成 `log_info!`，diff 会淹没真正的
//! 逻辑改动（而逐字核对正是迁移验收的手段）。保留同名函数让**调用点一字不改**。
//!
//! ── `verbose` 为什么也走 info ────────────────────────────────
//! 网关的 `verbose` 是「详细日志」档（默认关闭、由设置页开关控制）。插件
//! 无法查询那个开关，也无法写进网关的 verbose 缓冲 —— 所以这里**降级成普通
//! 日志**。代价是详细日志会进运行日志页（比以前吵一点），但比丢掉排障线索好。
//! 等协议加上 `log.level = "verbose"` 之后再恢复分档。

// 这一层是「保留网关调用点原样」的兼容外壳，不是每家都会用到全部四个入口。
#![allow(dead_code)]

use agent2api_provider_sdk::{log_error, log_info, log_warn};

/// 一条普通日志。
pub fn log(tag: &str, message: &str) {
    log_info!("{tag} {message}");
}

/// 一条详细日志（见模块头：当前降级为普通日志）。
pub fn verbose(tag: &str, message: &str) {
    log_info!("{tag} {message}");
}

/// 一条警告。
pub fn warn(tag: &str, message: &str) {
    log_warn!("{tag} {message}");
}

/// 一条错误。
pub fn error(tag: &str, message: &str) {
    log_error!("{tag} {message}");
}

/// 当前毫秒时间戳（网关 `logging::now_ms` 的等价物）。
pub fn now_ms() -> i64 {
    crate::http::now_ms()
}

/// 「终端那一行」的插件侧等价物（网关 `logging::console_line`）。
///
/// 网关那版直接打到启动它的终端（用户看得见，不落运行日志）。插件没有终端
/// —— 它的 stdout 是 IPC 通道，写一行就会破坏协议。所以这里降级成普通日志：
/// 内容一字不改，只是落点从终端变成运行日志页。
pub fn console_line(tag: &str, message: &str) {
    log_info!("{tag} {message}");
}
