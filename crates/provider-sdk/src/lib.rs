//! Agent2API 提供商插件 SDK。
//!
//! ── 作者要做什么 ────────────────────────────────────────────
//! 实现 [`Provider`] trait，然后：
//!
//! ```ignore
//! fn main() {
//!     agent2api_provider_sdk::serve(vec![Box::new(MyProvider)]);
//! }
//! ```
//!
//! 网关就会在 `init` 里拿到你的 provider，之后按能力位调你的方法。
//!
//! ── 作者**不该**看见的东西（都由本 crate 处理）────────────────
//!   - 分帧、JSON 编解码、base64；
//!   - 背压窗口与 `credit` 归还；
//!   - `abort` 的取消传播；
//!   - 日志回传（用 [`log`] 宏，别写 stderr）；
//!   - 账号读写（用 [`ctx::PluginCtx`] 的回调，别自己开库）。
//!
//! ── 一条硬约束：读循环绝不 await 业务 ────────────────────────
//! 插件会在处理请求的过程中回调网关（`store.get` 等）。如果读循环在等
//! 业务完成，那条回调的应答就永远读不到 —— 自己把自己锁死。因此
//! [`serve`] 的结构是：
//!
//! ```text
//! 读循环 ──读到请求──→ 派发到独立 task ──→ 应答/流式帧写回（带锁的写队列）
//!    ↑                                          │
//!    └────── 读到回调应答 ──→ 唤醒 pending oneshot ┘
//! ```
//!
//! 读循环只做「分类 + 投递」，一次 await 都不带业务语义。

mod checkin;
pub mod ctx;
mod errors;
pub mod images;
mod log_context;
pub mod provider;
pub mod retry;
pub mod serve;
mod serve_options;
pub mod timeouts;

pub use checkin::CheckinResult;
pub use ctx::{PluginCtx, StoreAccount, StoreError, StoreProxy};
pub use errors::control_error;
pub use provider::{
    local_precondition, split_local_precondition, split_unsupported, unsupported, BoxProvider,
    ChatPlan, ConversationSink, Provider, ProviderMeta, SendFuture, LOCAL_PRECONDITION_PREFIX,
    SCHEDULES_LIST_METHOD, UNSUPPORTED_PREFIX,
};
pub use retry::{RetryDecision, RETRY_DECISION_METHOD};
pub use serve::{serve, serve_blocking, serve_blocking_with, serve_with, ServeOptions};

/// 打一条日志到网关的运行日志页。
///
/// ── 为什么不直接用 `eprintln!` ────────────────────────────────
/// 三个理由，都踩过：① Windows GUI 进程没有控制台，`eprintln!` 写进虚无；
/// ② 网关的日志页按级别过滤、有容量上限，直接写 stderr 会绕过；
/// ③ stderr 是「插件崩了」这类兜底诊断的通道，业务日志混进去会淹没它。
///
/// 用法与 `log`/`tracing` 的宏接近，但不引入那两个依赖（插件作者可能已经
/// 用了别的日志库，我们只提供最小可用的一条路）。
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::serve::emit_log("info", format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        $crate::serve::emit_log("debug", format!($($arg)*))
    };
}

/// 打一条警告级日志（见 [`log_info!`]）。
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::serve::emit_log("warn", format!($($arg)*))
    };
}

/// 打一条错误级日志（见 [`log_info!`]）。
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::serve::emit_log("error", format!($($arg)*))
    };
}
