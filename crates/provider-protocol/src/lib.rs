//! Agent2API 提供商插件协议。
//!
//! ── 这个 crate 解决什么问题 ──────────────────────────────────
//! 网关（`agent2api-server`）与提供商实现原本编译在同一个进程里，靠
//! `ProviderAdapter` 这个 Rust trait 互相认识。改成插件架构之后，提供商实现
//! 由**用户安装的独立二进制**提供，trait 调用必须跨进程 —— 于是那些「既是
//! 网关概念、也是提供商概念」的类型（请求计划、错误分类、模型记录、凭证
//! 形态……）需要一份**双方共同编译**的定义。就是这里。
//!
//! ── 谁依赖它 ────────────────────────────────────────────────
//!   - 网关侧：`plugin_host`（进程监管 + RPC）与 `RpcAdapter`（trait 实现）；
//!   - 插件侧：`agent2api-provider-sdk`，以及每个提供商包。
//!
//! 因此本 crate 的依赖面刻意压到最小：serde / serde_json / base64，外加
//! tokio 的 `io-util`（只为分帧的 AsyncRead/AsyncWrite，不带运行时）。
//! **不要**把 reqwest、rusqlite、axum 加进来 —— 插件作者不该为了拿一个结构体
//! 定义而被迫拉进整套网关依赖树，那会让「写一个 provider」从一天变成一周。
//!
//! ── 协议长什么样（一句话）───────────────────────────────────
//! 两个进程用 stdin/stdout 通信，**一行一个 JSON**，双向对称：
//! 每一方都能发 `{id, method, params}`，另一方回 `{id, result}` 或
//! `{id, error}`。流式回复先发 `head`，再发若干 `chunk`，最后以
//! `{id, result: null}` 收尾。详见 [`frame`] 与 [`methods`] 的模块头。
//!
//! ── 版本 ────────────────────────────────────────────────────
//! [`PROTOCOL_VERSION`] 是**唯一**的兼容判据，握手时双方交换；不一致一律
//! 拒绝加载并提示升级插件包。加字段（`Option` / 带默认值）不需要升版本；
//! 改语义、删字段、改必填项必须升。

pub mod account;
pub mod chat;
pub mod frame;
pub mod limits;
pub mod methods;
pub mod model;
pub mod provider;
pub mod proxy;

pub use account::{
    AccountSecret, AccountView, CheckinOutcome, CheckinReport, LoginCallback, LoginCancel,
    LoginField, LoginFlow, LoginMethod, LoginPoll, LoginStart, NormalizeInput, NormalizeSource,
    NormalizedAccount, UsageReport,
};
pub use chat::{
    ChatRequest, ConversationRequest, CooldownModels, ErrorClass, ReasoningPatch, RetryAdvice,
    StreamHead, UpstreamKind,
};
pub use frame::{decode_chunk, encode_chunk, read_frame, write_frame, Frame, FrameKind, WireError};
pub use limits::{
    DEFAULT_WINDOW_BYTES, FRAME_OVERHEAD_BYTES, MAX_CHUNK_BYTES, MAX_LINE_BYTES, PROTOCOL_VERSION,
};
pub use model::{CatalogMeta, ModelList, ModelRecord, RefreshOutcome};
pub use provider::{
    signing_payload, AccountFeatures, AdminPanel, Capabilities, CheckinInfo, CheckinMode,
    CheckinUiHook, DefaultLimiter, DefaultLimiterAction, DefaultLimiterKind, PackManifest,
    PackProvider, PackSignature, PackUiPage, ProviderId, ProviderInfo,
};
pub use proxy::WireProxy;

/// 握手时网关告知插件的运行环境。
///
/// 这些值插件**只读**，用来决定「往哪写文件」「走不走系统代理」这类行为。
/// 每一项都给了插件一个明确的答案，省得它自己去猜 `%APPDATA%` 之类的路径 ——
/// 猜错的后果是插件把状态写到了网关备份/恢复覆盖不到的地方。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct HostEnv {
    /// 网关的数据目录（插件可以在这里建自己的子目录）。
    ///
    /// **凭证不在这里**：账号由网关持有，插件通过 `store.*` 回调读写，
    /// 见 [`methods`] 的账号面。
    pub data_dir: String,
    /// 网关版本（用于日志与「这个包要求更新的网关」判定）。
    pub host_version: String,
    /// 目标平台三元组（`x86_64-pc-windows-msvc` 这类）。
    pub target: String,
    /// 网关自己的出网代理（`""` = 直连，`"direct"` = 显式直连，其余是 URL）。
    ///
    /// 插件**自己发起的**请求若没被指定账号级代理，就用它。注意这是
    /// 「网关选定的出口」，不是操作系统代理 —— 两者的区别见网关的
    /// `core::egress`。
    pub proxy: String,
    /// 是否允许插件写调试报文（网关的调试模式开关）。
    pub debug_traffic: bool,
    /// 网关监听的 HTTP 端口（`0` = 未知）。
    ///
    /// ── 为什么插件需要它 ────────────────────────────────────────
    /// 回调型网页登录（LobsterAI / Accio / CodeArts / Antigravity）的
    /// `redirect_uri` 必须指向**网关自己**的 loopback 地址，而那个地址由
    /// 适配器在发起登录时本地拼出来。端口在进程生命周期内不变，握手时给一次
    /// 就够 —— 插件不必去猜 `%PORT%` 之类的东西。
    ///
    /// 旧版网关不带这个字段（`0`），插件据此退回「拿不到回调地址」的分支。
    #[serde(default)]
    pub loopback_port: u16,
}
