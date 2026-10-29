//! CodeArts 提供商（华为云 AI 代码助手 / snap-access）：账号管理 **+ 推理转发**。
//!
//! ── 上游长什么样 ────────────────────────────────────────────
//!   - 对话：`POST https://snap-access.cn-north-4.myhuaweicloud.com/api/v2/chat/completions`，
//!     SSE，body 是 OpenAI 形状但**必须带三个模型头**（`model-id` / `model-name` /
//!     `x-model-id`），福利模型还要 `maas_type: benefit`。
//!   - 鉴权：**SDK-HMAC-SHA256** 签名（AK/SK + STS 临时凭据，见 `signer.rs`），
//!     签名覆盖到出门那串字节 —— 所以模型名、会话 id、改写过的 `tools` 都要先定。
//!   - 会话：上游对每个账号只允许 **3 路并发**对话，靠
//!     `PUT /snap-manager/v1/chat-session/heartbeat?status=busy|idle` 记账
//!     （见 `session.rs`）。
//!   - 登录：OAuth 授权码 + PKCE + **DPoP**（`sts.cn-north-4.myhuaweicloud.com`），
//!     回调落在本机 loopback（见 `login.rs`）。
//!   - 额度：订阅统计（`/snap-manager/v1/statistics/plugin`）+ 福利网关
//!     （`opengw.developer.huaweicloud.com`）两份账。
//!   - 「签到」：ops 福利的每日登录送积分（`/v1/ops/delivery` → `claim` → `confirm`
//!     → 回读确认），另有一次性新人注册礼。
//!
//! ── 为什么走会话式转发（`stateful = true`）──────────────────
//! 无状态路径假设「网关自己发请求、插件只回答怎么构造」。本家两件事做不到：
//!   1. 要先占一个上游会话槽（心跳 busy/idle），那是**跨请求**的状态；
//!   2. 失败判据在**流里**（HTTP 200 + 一帧 `InferHub.4291.200 / insufficient
//!      quota`），必须由插件在首包门里判掉才能触发换账号。
//! 所以整条发送链收进 `conversation`。
//!
//! ── 与网关那份的差别（迁移时改的）───────────────────────────
//! | 网关 | 插件 |
//! |---|---|
//! | `ProviderAdapter` + `AccountStore` | `Provider` + `PluginCtx` |
//! | `ForwardOutcome::Stream/Completion` | `ConversationSink`（head + chunk） |
//! | `RequestTelemetry` 抓包 / 用量 | **空实现**（见 `telemetry` 模块头） |
//! | `update_codearts_credentials_if_current` | `store::save`（比较再写，见 `refresh.rs`） |
//! | 账号面（`core::account_store::codearts_accounts`） | `accounts.rs` |
//! | 福利台账在账号记录的 `codeartsWelfare` 字段 | `ctx.ledger_*`（同名 kind） |
//! | 新手任务结算记忆在 `onboardingSettled` 字段 | `ctx.ledger_*`（同名 kind） |
//! | 登录回调挂在网关端口 + 进程级 `LOOPBACK_PORT` | `login.rs` 自绑 loopback 端口 |
//! | `catalog_cache` 落盘 | **不迁**（见 `models::store_catalog`） |
//!
//! ── 已知的能力差（如实列出）─────────────────────────────────
//!   1. 调试抓包与用量上报没有协议挂点（`telemetry` 是空实现）；
//!   2. 目录缓存不落盘 —— 插件进程重启后首轮必须真打一次上游才能列模型；
//!   3. 面板上「并发上限」读的是账号 secret 里的 `maxConcurrent`（插件读不到
//!      记录的普通字段），没写就用默认 3；
//!   4. `checkin` 没有 `manual` 维度（本家的定时签到本来就不接，见 `adapter.rs`）。
//!
//! ── panic=abort ────────────────────────────────────────────
//! 本模块在对话链路上，绝不 unwrap/expect/panic。

pub mod adapter;
pub(crate) mod accounts;
pub(crate) mod authorization;
pub(crate) mod balance;
pub(crate) mod callback_server;
pub(crate) mod chat;
pub(crate) mod credentials;
pub(crate) mod dpop;
pub(crate) mod login;
pub(crate) mod models;
pub(crate) mod oauth;
pub(crate) mod onboarding;
pub(crate) mod redact;
pub(crate) mod refresh;
pub(crate) mod response_stream;
pub(crate) mod session;
pub(crate) mod signer;
pub(crate) mod size_gate;
pub(crate) mod stream_fault;
pub(crate) mod tool_names;
pub(crate) mod welfare;

/// 本家的 provider id（落盘契约）。
pub const PROVIDER_ID: &str = "codearts";
