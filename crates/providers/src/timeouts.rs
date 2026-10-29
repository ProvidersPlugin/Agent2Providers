//! 每次调用的超时快照由 SDK 隔离保存；旧网关未下发时使用默认值。

pub use agent2api_provider_sdk::timeouts::{current, Timeouts};
