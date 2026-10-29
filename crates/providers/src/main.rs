//! Agent2API 官方提供商插件包。
//!
//! ── 这个 crate 是什么 ───────────────────────────────────────
//! 一个**独立的可执行文件**：网关 spawn 它，通过 stdin/stdout 的 JSON 行
//! RPC 调用它。它包含 21 家提供商的实现（从网关的 `core/providers/` 搬来）。
//!
//! ── 为什么独立成包而不是留在网关里 ──────────────────────────
//! 三条理由，按重要性排：
//!   1. **稳定性**：网关 release 是 `panic = "abort"`，一家 provider 里的
//!      一个越界索引就能带走整个桌面应用。拆出去之后它只带走这个进程。
//!   2. **可替换**：用户装什么包、装哪个版本，与网关的发版节奏解耦。
//!   3. **可维护**：加一家 provider 只改这个仓库，不必碰网关的
//!      `ProviderKind` / `PROVIDERS` / 账号层 / 前端表单。
//!
//! ── 目录约定 ────────────────────────────────────────────────
//! 每家一个子模块，与网关原来的 `core/providers/<name>/` 一一对应：
//! ```text
//! src/
//!   main.rs        入口（本文件）
//!   providers/
//!     mod.rs       注册表：把各家实现装进 Vec<Box<dyn Provider>>
//!     demo/        示例实现（验证整条链路；同时是写新 provider 的模板）
//!     workbuddy/   以下逐家从网关搬来
//!     raccoon/
//!     …
//! ```
//!
//! ── 迁移进度 ────────────────────────────────────────────────
//! 目前只有 `demo`（示例）。其余 21 家按计划 P2–P4 逐批搬入 ——
//! 搬运时**代码基本原样**，只把 `crate::server::…` 的路径改成
//! `agent2api_provider_sdk::…`，因为共享类型已经在协议 crate 里了。

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// ── 关于上面这条 deny ───────────────────────────────────────
// 插件进程崩溃不会带走网关，但会打断所有在途请求。所以这里比网关更严：
// 连 `unwrap` 都不允许（网关只是「尽量不写」，这里是编译期禁止）。
// 确实需要的地方用 `guard()` 显式兜住并记日志。

mod beijing;
mod content_block;
mod crypto;
mod dpop;
mod errors;
mod flight;
mod headers;
mod http;
mod ids;
mod json;
mod limits;
mod model_rules;
mod paths;
mod proxy_util;
mod queue;
mod quota;
mod logging;
mod onboarding;
mod providers;
mod readiness;
mod reasoning;
mod response_stream;
mod sse;
mod sse_events;
mod stall;
mod telemetry;
mod store;
mod timeouts;
mod timeutil;
mod workbuddy_shared;

fn main() {
    agent2api_provider_sdk::serve_blocking_with(
        providers::registry(),
        agent2api_provider_sdk::ServeOptions {
            on_init: Some(|env| paths::init(&env.data_dir)),
        },
    );
}
