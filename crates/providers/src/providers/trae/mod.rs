//! Trae（字节跳动 AI IDE）提供商。
//!
//! ── 这一家在网关眼里长什么样 ──────────────────────────────
//! 上游不是 OpenAI 兼容端点，而是 `trae-api-cn.mchost.guru` 上的
//! `POST /api/agent/v3/llm_utils_chat`：请求体是「白名单重建」的自定义信封
//! （见 `payload.rs`），响应恒是 SSE 且**业务错误塞在 200 的流里**
//! （见 `stream.rs` 与 `errors.rs`）。所以本家与 accio / codearts 一样走
//! stateful 路径，由自己掌控出站与流解析。
//!
//! 与 codearts 最大的差别是**这里没有签名层**：鉴权就是一个
//! `Cloud-IDE-JWT <token>` 加一批身份头（`headers.rs`），唯一的密码学是
//! 登录时的 PKCE(S256)。省掉的正是 codearts 那边最贵的一块。
//!
//! ── 只支持 SOLO 这一条通道，是有意的 ──────────────────────
//! `llm_utils_chat` 只认 `function=solo_work_lite`。Trae 的另外两条通道
//! （IDE 的 `/api/agent/v3/create_agent_task`、`llm_raw_chat` 的 `solo_agent`）
//! 一条要 AES-256-GCM 加密请求体、一条被上游限成「只能由 agent 任务内部调用」，
//! 都不适合做透明代理。目录里属于那两条通道的配置会在解析阶段直接过滤
//! （`errors::config_is_solo_agent_only`），因为它们在本通道必定流内 4001。
//!
//! Intl（`core-normal.trae.ai` 的 `chat_sessions` → `events` 两步握手、
//! 累积 thought、`Origin` 绑定、无 tools）是**另一套协议**，单列里程碑，
//! 不与本模块共用信封 —— 因此 `accounts.rs` 在入口就把 Intl 凭据拒掉。
//!
//! ── 迁移到插件时改了什么 ────────────────────────────────────
//! | 网关 | 插件 |
//! |---|---|
//! | `pub mod` 各子模块 | `mod`（只有 `adapter` / `accounts` 对外） |
//! | `ProviderKind::Trae` | `TraeProvider` + `PROVIDER_ID` |
//! | `AccountStore` 的 `add_trae_account` / `to_trae_public_account` | `accounts.rs` |
//! | `core::login::trae` 的待办表 | `login.rs` 的进程内表 |
//! | `login.rs`（Session 编排） | `session.rs`（同名同内容） |
//!
//! 子模块清单（一眼看清谁管什么）：
//! ```text
//!   adapter       Provider 实现：分错误 / 取令牌 / 刷目录 / 账号面 / 登录入口
//!   accounts      账号归一化、公开视图、稳定身份
//!   forward       有状态转发（首包门 + 翻帧 + 冷却标记）
//!   session       一轮网页登录的编排（绑端口 → 换证 → 读身份）
//!   login         进程内的登录表（start / poll / callback / cancel）
//!   callback_server  本机 loopback 回调监听器（一次登录一个随机端口）
//!   oauth         授权地址 / guidance / 授权码换证
//!   refresh       refreshToken 续期链（多候选 host + ClientID）
//!   credentials   凭据形态与 JWT 读数
//!   profile       GetUserInfo 与身份合并
//!   models        目录（get_detail_param）与模型表
//!   payload       出站信封（白名单重建）
//!   stream        SSE 方言解析（无 [DONE]、usage 欠一帧）
//!   errors        状态码 + 业务码 → ErrorKind
//!   headers       身份头与客户端画像
//!   http          本家的出站薄封装
//!   usage         额度 / 用量读数
//!   checkin       每日签到
//!   device        设备密钥对（PKCS#8 PEM）
//! ```
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本家全部文件零 unwrap / expect / panic。

pub mod adapter;
pub(crate) mod accounts;
pub(crate) mod callback_server;
pub(crate) mod checkin;
pub(crate) mod credentials;
pub(crate) mod device;
pub(crate) mod errors;
pub(crate) mod forward;
pub(crate) mod headers;
pub(crate) mod http;
pub(crate) mod login;
pub(crate) mod models;
pub(crate) mod oauth;
pub(crate) mod payload;
pub(crate) mod profile;
pub(crate) mod refresh;
pub(crate) mod session;
pub(crate) mod stream;
pub(crate) mod usage;

/// 上游主机。CN 与 SOLO 共用；`api.trae.cn` / `api.trae.com.cn` 是
/// OAuth 与积分那条链的 host（见 `credentials.rs`）。
pub const AGENT_BASE_URL: &str = "https://trae-api-cn.mchost.guru";
pub const CHAT_PATH: &str = "/api/agent/v3/llm_utils_chat";
pub const MODELS_PATH: &str = "/api/ide/v1/get_detail_param";

/// 提供商 id（注册表与 `pack.json` 用同一条）。
pub const PROVIDER_ID: &str = "trae";

/// 签到结果行 → 协议报告（形状见 `CheckinReport` 的文档）。
///
/// ── 为什么判据是 `success` 而不看 `code` ────────────────────
/// 本家的 claim 对**当日已签过**的账号回 `code:0`（假成功），所以
/// `checkin.rs` 已经把「真领到」翻成了 `success:true`（回查确认才给）。
/// 这里只做翻译，不再判一次 —— 两处各判一套的话，「今日已签」会时而算成功、
/// 时而算失败。
pub(crate) fn checkin_report(value: &serde_json::Value) -> agent2api_provider_protocol::CheckinReport {
    use agent2api_provider_protocol::{CheckinOutcome, CheckinReport};
    let success = value
        .get("success")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let message = value
        .get("msg")
        .and_then(serde_json::Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let already = value
        .get("alreadyCompleted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let outcome = if success {
        CheckinOutcome::Claimed
    } else if already {
        // 今日已签：记为 Done（不再重试），不是失败
        CheckinOutcome::Done
    } else if message
        .as_deref()
        .is_some_and(|text| text.contains("不支持") || text.contains("无此活动"))
    {
        // 国际版没有签到活动（`checkin::run` 会明说）
        CheckinOutcome::Ineligible
    } else {
        CheckinOutcome::Failed
    };
    CheckinReport {
        outcome,
        // 数额是上游**报出的当日奖励**，不是到账余额（见 `checkin.rs` 模块头）
        credit: if success {
            value
                .get("reward")
                .or_else(|| value.get("credit"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        } else {
            None
        },
        streak: None,
        message,
    }
}
