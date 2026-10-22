//! KukuAI 提供商：会话式转发（建会话 → 分配算力 → SSE 翻译）。
//!
//! ── 上游长什么样（依据客户端 app.asar + 两个参考实现实测）──
//! KukuAI 桌面端是标准 Electron 套壳（`kuku.baidu.com` 网页），**没有任何可外部
//! 调用的本地 HTTP API**；服务端认证只需要三个 Cookie：`BDUSS` / `STOKEN` /
//! `gfprotpl=genflowpro`（后一个恒附加，不在账号里存）。
//!
//! ── 对话链路（kuku2api 2026-09-13 实测）────────────────────
//! ```text
//!   1. GET  /api/genflowpro/common/userreport → {bdstoken, uinfo, uk}
//!      （会话三件套，600 秒过期，进程级缓存见 `session`）
//!   2. POST /wenchain/genflowpro/sendmsg → {session_id, reply_id}
//!   3. POST /wenchain/genflow/idallochstr（分配算力）+ sessionswitch（容错）
//!   4. POST /wenchain/genflowpro/sse/getchatcontent（SSE）→ 流式文本
//! ```
//! 上游**没有多轮接续接口**：每次请求新建会话，多轮历史拍平为单条 prompt
//! （`chat::build_prompt`，与参考实现同口径）。
//!
//! ── 会话是**请求内**的（与 CatPaw / Trae 的差别）───────────────
//! 同样是 `stateful: true`，但 KukuAI 的会话建完即用、用毕即弃 —— 不需要
//! 会话注册表 / 轮次状态机，`chat::run_chat` 单函数完成全部时序。
//!
//! ── 与网关那份的差别（迁移时改的）───────────────────────────
//! | 网关 | 插件 |
//! |---|---|
//! | `AccountStore` | `PluginCtx` + `store` |
//! | `ForwardOutcome::Stream/Completion` | `ConversationSink`（head + chunk） |
//! | `RequestTelemetry` 抓包 | **不迁**（插件的 `debug_traffic` 由网关侧记） |
//! | `engine.rs` 的 DLL 签名参数体 | 退化成「最简表单」（见 `engine` 模块头） |
//! | `models` 落盘缓存 | 网关的 `rpc_cache` |
//!
//! ── 凭证：三个 Cookie，**无法刷新** ─────────────────────────
//! BDUSS 是百度通行证登录态（不是 JWT，没有 refreshToken 概念），上游也没有
//! 刷新接口 —— `refresh: false`，401 后用户重新登录 / 重新导入即可。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本目录零 unwrap/expect/panic，不持任何锁跨 await。

mod balance;
mod chat;
mod checkin;
mod credentials;
mod engine;
mod http;
mod login;
mod models;
mod session;

use std::collections::BTreeMap;

use serde_json::Value;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ConversationSink, PluginCtx, Provider, ProviderMeta, SendFuture};

use crate::errors::GatewayError;

/// 主站（对话 / 目录 / 积分全在这一个域上）
pub const BASE_URL: &str = "https://kuku.baidu.com";

/// 客户端硬编码的 app_id（`123971023`；另一个 `123971202` 是云盘流媒体专用的，
/// 别混用 —— 缺了这个值 freepoint / sendmsg 会 400 `params error`）
pub const APP_ID: i64 = 123971023;

/// 客户端硬编码的 channel
pub const CHANNEL: &str = "chunlei";

/// 对话链路 query（kuku2api 实测的**网页形态**：`clienttype=400&web=1&version=1.4.4`）。
///
/// 为什么对话不用桌面客户端形态（`401/1.6.7`）：sendmsg / getchatcontent 的
/// 实测记录来自 kuku2api（网页形态），桌面形态只有 freepoint 的实测。
/// 两组接口各认各的形态，照抄各自的实测值。
pub const WEB_QUERY: &str = "clienttype=400&app_id=123971023&web=1&channel=chunlei&version=1.4.4";

/// 出站 User-Agent（kuku2api 实测可用；上游对 UA 敏感，不能用 reqwest 默认值）
pub const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 ",
    "(KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36"
);

/// 默认模型（上游自动选路；未知模型名显式回退它，避免上游静默降级）
pub const DEFAULT_MODEL: &str = "auto";

/// 思考模式默认档（参考实现固定 3；本模块不接思考等级绑定）
pub const DEFAULT_THINK_MODE: i64 = 3;

/// 本家的 provider id（账号记录与日志都用它）
pub fn kuku_id() -> &'static str {
    "kuku"
}

/// 把一组头转成协议里的 `BTreeMap`（小写键）。
pub(crate) fn header_map(headers: &[(String, String)]) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(key, value)| (key.to_ascii_lowercase(), value.clone()))
        .collect()
}

/// KukuAI 提供商（无状态单例：会话是请求内的，凭证在账号存储里）。
pub struct KukuProvider;

impl Provider for KukuProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(kuku_id(), "KukuAI 库库")
            .with_capabilities(proto::Capabilities {
                chat: true,
                // 上游是「建会话 → 分配算力 → SSE」三步时序
                stateful: true,
                // BDUSS 是 Cookie 登录态，没有刷新接口
                refresh: false,
                // `/wenchain/genflowpro/model_list`
                model_refresh: true,
                // `/bizapi/gfpro/getgfvipremain`
                usage: true,
                // freepoint 每日任务（`checkin`）
                checkin: true,
                claims: false,
                // 百度通行证登录页（注入脚本取回 Cookie）
                web_login: true,
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: true,
            })
            .with_login(vec![
                proto::LoginMethod::Browser {
                    button: "网页登录".to_string(),
                    hint: Some(
                        "打开百度通行证登录页，登录成功后自动取回登录态".to_string(),
                    ),
                },
                proto::LoginMethod::Token {
                    label: "Cookie".to_string(),
                    placeholder: "BDUSS=…; STOKEN=…（也可粘贴 Cookie 编辑器导出的 JSON）"
                        .to_string(),
                    extra_fields: Vec::new(),
                },
                proto::LoginMethod::Desktop {
                    button: "导入本机登录态".to_string(),
                    hint: Some("读取 KukuAI 桌面客户端已登录的 Cookies".to_string()),
                },
            ])
            .with_models(models::list())
    }

    /// 模型清单：**远程目录优先**，未拉到用静态兜底。
    fn models<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async move {
            Ok(proto::ModelList {
                models: models::list(),
                meta: proto::CatalogMeta {
                    remote: models::remote_refreshed(),
                    refreshed_at: models::last_refreshed_at(),
                },
            })
        })
    }

    /// 会话式转发入口：转调 `chat::run_chat`。
    ///
    /// ── 分工（哪些判断不属于这里）──────────────────────────────
    ///   账号选路、限额冷却、provider 轮询都在编排层；
    ///   协议时序（建会话 / 分配算力 / SSE 翻译）都在 `chat`。
    ///   本函数只做**装配**。
    fn conversation<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ConversationRequest,
        sink: ConversationSink<'a>,
    ) -> SendFuture<'a, ()> {
        Box::pin(chat::run_chat(ctx, req, sink))
    }

    /// 一律 Fatal：上游业务错误在 `chat::run_chat` 内部已经归一到
    /// `GatewayError`（状态码与文案按参考实现的口径映射好），本方法在
    /// 正常路径上走不到。
    fn classify(&self, status: u16, error_body: &Value) -> proto::ErrorClass {
        let message = error_body
            .get("message")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误")
            .to_string();
        proto::ErrorClass::Fatal {
            status,
            message,
            upstream_code: None,
        }
    }

    /// 凭证：从账号记录取 BDUSS（返回 token 字符串 = bduss）。
    ///
    /// 本家**没有**环境变量旁路与桌面端实时登录态兜底（桌面端导入会落成一条
    /// 账号记录）—— 因此 `account` 为空时也走账号记录（本家组内最优账号）。
    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let credentials = credentials::snapshot_for(ctx, account)
                .await
                .map_err(GatewayError::into_message)?;
            Ok(credentials.bduss)
        })
    }

    /// 拉取远程模型目录（`GET /wenchain/genflowpro/model_list`）。
    ///
    /// 凭证取当前账号（与转发同一套 Cookie）；没有可用登录态时返回
    /// `unchanged()`（不是错误）。`force` 一路透传：`false` 走 TTL 早退
    /// （自动路径），`true` 真打上游（用户手动点了「刷新模型清单」）。
    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(models::refresh(ctx, account, force))
    }

    /// 查余额（`/bizapi/gfpro/getgfvipremain`）。
    ///
    /// 与 CatPaw 不同，KukuAI **没有独立查询凭证** —— 转发用的三个 Cookie
    /// 直接能查，因此每个账号都能查到。
    fn usage<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, Value> {
        Box::pin(async move {
            balance::query_usage(ctx, account)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    /// 每日签到：f
