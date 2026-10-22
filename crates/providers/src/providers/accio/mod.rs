//! Accio 提供商（`accio` 国际版 / `accio-cn` 国内版，同一份实现按地区参数化）：
//! 账号管理 **+ 推理转发**。
//!
//! ── 上游长什么样（依据：Accio Work 桌面端安装包的 app.asar）──────
//!   - 业务网关：`https://phoenix-gw.alibaba.com`（两地区共用），鉴权**不走
//!     Authorization**：GET 把 `accessToken` 拼 query、POST 放进 body。
//!   - 推理网关：`POST {gw}/api/adk/llm/generateContent?sg_k=<md5(requestId)>`，
//!     SSE，body 是 **Gemini 风格**的 protobuf-JSON（`contents` /
//!     `system_instruction` / `tools`），账号凭证放在 body 的 `token` 字段里。
//!   - 登录：OAuth 2.0 授权码 + PKCE（`client_id=accio-work`），登录站点按地区
//!     分（`www.accio.com` / `www.accio-ai.com`）。
//!   - 额度：`GET /api/entitlement/quota` → `{usagePercent, refreshCountdownSeconds}`。
//!   - **没有签到**：上游没有那个活动（桌面端全包检索无「签到 / checkin」）。
//!
//! ── 为什么走会话式转发（`stateful = true`）──────────────────
//! 无状态路径假设「请求体由通用层序列化后原样发出、响应是 OpenAI 的 SSE」。
//! Accio 两条都不满足（请求体要先转成 Gemini 信封、响应要拆 ADK 的帧）。
//! 因此这里实现 `conversation`，把「构造 → 发送 → 翻译」收进 `chat.rs`。
//! 与 Qoder 的差别：Accio 的鉴权只是「body 里放 token」，没有签名与编码，
//! 所以不需要 `cosy.rs` 那种签名层。
//!
//! ── 两个地区 = 两个 provider ────────────────────────────────
//! 与 `autoclaw::region` / Cline 两池同一条思路：地区做成账号上的字段，界面与
//! 选路就分不清「哪条账号走哪个站点」。所以这里两个静态实例，共用同一份实现。
//!
//! ── 未验证项（接上线后第一次真实调用要盯的）──────────────────
//!   1. `utdid` / `version` / `x-package-region` 三个头是否被上游校验
//!      （桌面端都带；本实现按账号生成稳定的 utdid，可被 `ACCIO_*` 环境变量
//!      覆盖）—— **已实测（2026-09）**：三者都不是鉴权项，缺了也能跑通；
//!      真正必带的是 `appKey`（见 `endpoints::DEFAULT_APP_KEY`）。
//!   2. `redirect_uri` 白名单是否放行插件的 loopback 地址（桌面端自己用的
//!      就是 `http://127.0.0.1:<port>/auth/callback`，形态一致，风险低）；
//!   3. 上游目录里 `visible: false` 的模型是否真的不可用（按不可用过滤）。
//!
//! ── 与网关那份的差别（迁移时改的）───────────────────────────
//! | 网关 | 插件 |
//! |---|---|
//! | `ProviderAdapter` + `AccountStore` | `Provider` + `PluginCtx` |
//! | `ForwardOutcome::Stream/Completion` | `ConversationSink`（head + chunk） |
//! | `RequestTelemetry` 抓包 / 用量 | **空实现**（见 `telemetry` 模块头） |
//! | `store.update_accio_credentials_if_current` | `store::save`（比较再写） |
//! | 账号面（`core::account_store::accio_accounts`） | `accounts.rs` |
//! | 登录回调挂在网关端口 | `login.rs` 的 loopback 监听器 |
//! | `catalog_cache` 落盘 | **不迁**（见 `models::restored_cache`） |
//! | `credentials_expiring` / `supports_default_model` | 协议无此位 |
//!
//! ── panic=abort ────────────────────────────────────────────
//! 本模块在对话链路上，绝不 unwrap/expect/panic。

pub mod adapter;
pub(crate) mod accounts;
pub(crate) mod auth;
pub(crate) mod balance;
pub(crate) mod callback_server;
pub(crate) mod chat;
pub(crate) mod credentials;
pub(crate) mod endpoints;
pub(crate) mod login;
pub(crate) mod models;
pub(crate) mod oauth;
pub(crate) mod protocol;
pub(crate) mod refresh;
pub(crate) mod stream;

/// 国际版的 provider id
///
/// `#[allow(dead_code)]`：两处真正用到它的是 `Region::provider_id()`（那是
/// 唯一的事实来源），这两个常量是给排障与将来的匹配分支留的**具名**形态。
#[allow(dead_code)]
pub const PROVIDER_ID_GLOBAL: &str = "accio";
/// 国内版的 provider id（理由同 [`PROVIDER_ID_GLOBAL`]）
#[allow(dead_code)]
pub const PROVIDER_ID_CN: &str = "accio-cn";
