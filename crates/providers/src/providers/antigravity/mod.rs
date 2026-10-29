//! Antigravity（Google 的 AI IDE）适配实现：账号 / token 刷新 / 模型目录 /
//! **聊天转发**。
//!
//! ── 上游长什么样（依据：规格 `_recon/antigravity-spec.md`，参考
//! `Acankao/Antigravity-Manager`（Rust/Tauri，主源）与 `Acankao/9router`（Node，
//! 交叉验证））──────────────────────────────────────────────────
//! Antigravity IDE 的推理走 **Google Cloud Code Assist**（`v1internal`），
//! **不是** `generativelanguage.googleapis.com`，也不是 OpenAI / Anthropic 协议：
//!
//! ```text
//!   站点     全球统一（没有地区参数、没有国内/国际双站点 —— 规格 §6）
//!   鉴权     Google OAuth 2.0（授权码 + loopback：粘贴 refresh token
//!             与网页登录两条入口）→ Authorization: Bearer {access_token}
//!   推理     POST {base}/v1internal:streamGenerateContent?alt=sse
//!             （chat 用 daily 基址；信封与翻译见本目录的 antigravity_*）
//!   目录     POST {base}/v1internal:fetchAvailableModels
//!   project  POST https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist
//!            （无 project 时再 onboardUser；两个调用固定走 prod，见 project.rs）
//! ```
//! 三个环境基址（`sandbox` → `daily` → `prod`，**不是地区**）：聊天流量取
//! `daily`（9router 的固定选择，见 `adapter.rs` 的说明；端点级 failover 本步
//! 未实现）；project 发现固定 prod。
//!
//! ── 迁移时改了什么（网关 → 插件）────────────────────────────
//! | 网关 | 插件 |
//! |---|---|
//! | `ProviderAdapter` + `UpstreamResponse::AntigravityGemini` | `Provider::build_chat` 返回 `ChatPlan{ upstream: Gemini }` |
//! | `core::protocol::antigravity_{outbound,schema,stream,signature_cache}` | 本目录同名文件（依赖收进 `protocol.rs`） |
//! | 回调落**网关端口** `/oauth-callback` + `core::login` 任务表 | 插件自绑 loopback 端口（`callback_server`）+ 本目录 `login.rs` |
//! | 账号记录在 `account_store::antigravity_accounts` | `store.get/update` 回调 + 本目录 `accounts.rs` |
//! | `catalog_cache` 落盘目录缓存 | 只剩内存（插件侧没有落盘表；重启后首轮真打上游） |
//! | `egress::client_for` / `client_for_system_proxy` | `crate::http::control_client` / `crate::http::control_client_system_proxy` |
//!
//! ── 为什么 `stateful = false` ───────────────────────────────
//! 上游是「一次 HTTP 请求 = 一次生成」，只是响应帧是 v1internal 信封
//! （`data: {"response":{…}}`）+ Gemini 字段路径。插件报
//! `UpstreamKind::Gemini`，网关据此选 `AntigravityToChatStream` 翻译层 ——
//! 与 Command Code 的 NDJSON、ZCode 的 Anthropic 同一处置。没有多步会话协议，
//! 用 `conversation` 只会把账号轮换 / 限额冷却 / 退避重试在插件里重写一遍。
//!
//! ── 交付边界（全部已接通）────────────────────────────────────
//! ```text
//!   ✅ 账号：粘贴 refresh token 添加（归一化 + 一次真实校验刷新）
//!   ✅ token 刷新：临期主动刷 + 401 后强制刷（单飞 + 比较再写）
//!   ✅ 模型目录：POST :fetchAvailableModels（Gemini / Claude / GPT-OSS）+ 内置兜底清单
//!   ✅ 聊天转发：build_chat 构造 v1internal 信封 → 网关翻译回 chat SSE
//!   ✅ 网页登录：Google OAuth 授权码 + 插件自绑 loopback 回调
//!   ✅ 余额：5 小时 / 每周额度窗口 + 订阅档
//! ```
//!
//! ── 已知能力差（协议没有挂点，如实记录）──────────────────────
//!   1. `outbound_reasoning`（请求日志的「上游等级」列）没有协议钩子 ——
//!      `reasoning_patch` 注入的 `reasoning_effort` 仍生效，只是日志里看不到；
//!   2. `credentials_expiring` 有对应位（`token.expiring`），已接；
//!   3. 调试抓包（`HostEnv.debug_traffic`）插件侧没有采集器 —— 抓包开关对本家
//!      无效；
//!   4. 用量上报（网关的 `telemetry`）在插件里是空实现，调用点保留；
//!   5. 思考签名：插件保留客户端回传的真实签名，Gemini / GPT-OSS 历史缺失时
//!      补官方哨兵。网关在响应缓存所在进程按调用 id 回填真实签名，避免两份
//!      进程内缓存互相不可见；没有缓存的旧会话仍由哨兵兜底。
//!
//! ── 子模块分工 ──────────────────────────────────────────────
//! ```text
//!   endpoints.rs    OAuth 常量 / v1internal 三个环境基址 / 方法名 / 请求头 / UA 约束
//!   credentials.rs  账号凭证（refreshToken 主 + accessToken 缓存）+ 临期判定
//!   oauth.rs        Google token 端点：刷新（form）+ 单飞 + 比较再写 + 换码
//!   project.rs      cloudaicompanionProject 发现（loadCodeAssist → onboardUser）
//!   models.rs       :fetchAvailableModels + 内置兜底 + 对外 id → 上游真名映射表
//!   login.rs        粘贴式归一化（1// 前缀 / 引号 / Bearer）+ 网页登录两条入口
//!   callback_server.rs  网页登录的本机回调监听器（一次登录一个随机端口）
//!   accounts.rs     账号归一化 / 公开视图 / 稳定身份
//!   adapter.rs      Provider 实现（转发 / 刷新 / 目录 / 余额 / 登录）
//!   balance.rs      余额读数（5 小时 / 每周额度窗口 + 订阅档）
//!   protocol.rs     协议辅助（帧 / SSE 行缓冲 / 取值函数）
//!   antigravity_outbound.rs   chat 请求 → v1internal 信封
//!   antigravity_schema.rs     工具 schema 清洗（Gemini 不接受 JSON Schema 全集）
//!   antigravity_signature_cache.rs  thoughtSignature 的两代有界缓存
//! ```
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本目录零 unwrap/expect/panic。
//! **不发 `x-goog-api-client`**（规格 §7.2：属于 IDE 的 JS 层，发了形成矛盾指纹）。

pub mod accounts;
pub mod adapter;
pub mod antigravity_outbound;
pub mod antigravity_schema;
mod antigravity_signatures;
pub mod antigravity_signature_cache;
pub mod balance;
pub mod callback_server;
pub mod credentials;
pub mod endpoints;
mod image_generation;
pub mod login;
pub mod models;
pub mod oauth;
pub mod project;
pub mod protocol;

// 注册表用的是 `adapter::ANTIGRAVITY`（见 `providers/mod.rs`），这里不再转导出


/// provider id（落盘契约，与网关内置家同名 —— 切换点见 `adapter_by_id`）。
pub const PROVIDER_ID: &str = "antigravity";

use agent2api_provider_protocol::WireProxy as ResolvedProxy;

/// 本家出网 Client 的选择：**显式配了代理就用它，否则跟随系统代理**。
///
/// ── 为什么与别家不同（别照抄这一条去别家用）────────────────────
/// 仓内默认口径是「直连就是直连」（`crate::http::control_client` 的直连分支显式
/// `.no_proxy()`）：转发出口由账号的代理配置决定，不被机器环境悄悄改写。
/// 那对国内可直连的上游是对的；本家的上游是 Google，多数网络里只有经代理
/// 才可达 —— 而用户机器上「已经能打开 Google 的那个代理」就写在系统设置里
/// （浏览器能打开授权页正是靠它）。因此本家的口径是**与浏览器一致**：
///   1. 账号显式配了代理（`proxy` 为 Some）→ 走它，优先级最高；
///   2. 没配 → [`crate::http::control_client_system_proxy`]（系统设置 + 环境变量）；
///   3. 系统也没配代理 → reqwest 探测不到，等价于直连，不引入新的失败面。
///
/// 登录链路（换码 / userinfo / 粘贴校验 / project 发现）同理：此刻账号还不
/// 存在，`proxy` 恒为 None，走第 2 条。
pub(crate) fn client_for(proxy: Option<&ResolvedProxy>) -> reqwest::Client {
    match proxy {
        Some(proxy) => crate::http::control_client(Some(proxy), "direct"),
        None => crate::http::control_client_system_proxy(),
    }
}
