//! KukuAI（百度文库「库库 AI / GenFlowPro」，`kuku.baidu.com`）提供商。
//!
//! ── 上游长什么样（依据客户端 app.asar + 两个参考实现实测，见 Acankao/）──
//! KukuAI 桌面端是标准 Electron 套壳（`kuku.baidu.com` 网页），**没有任何可外部
//! 调用的本地 HTTP API**；服务端认证只需要三个 Cookie：
//! `BDUSS` / `STOKEN` / `gfprotpl=genflowpro`（后一个恒附加，不在账号里存）。
//! 本机登录态落在 `%APPDATA%\baidugenflowpro\Network\Cookies`（SQLite），
//! 且**不是** Chromium 加密存储（`encrypted_value` 长度 0，`value` 列明文），
//! 无需 DPAPI —— 与 CatPaw 的 `auth.json` 一样可以做「桌面端实时导入」。
//!
//! ── 对话链路（kuku2api 2026-09-13 实测，本模块照抄其协议）──────
//! 上游不是 OpenAI 协议，而是「建会话 → 分配算力 → SSE 流式」三步走：
//!   1. `GET  /api/genflowpro/common/userreport` → `{bdstoken, uinfo, uk}`
//!      （会话三件套，600 秒过期，会话级缓存见 `session.rs`）；
//!   2. `POST /wenchain/genflowpro/sendmsg` → `{session_id, reply_id}`；
//!   3. `POST /wenchain/genflow/idorl`（分配算力，注意是 `/wenchain/genflow/`
//!      不是 genflowpro）+ `sessionswitch`（浏览器会调，容错）；
//!   4. `POST /wenchain/genflowpro/sse/getchatcontent`（SSE）→ 流式文本。
//! SSE 事件只有四类：`TEXT_BLOCK_DELTA`（增量）/ `REPLY_END` / `DIALOGUE_END`
//! （结束）/ `ERROR`。上游**没有多轮接续接口**：每次请求新建会话，多轮历史
//! 拍平为单条 prompt（`chat.rs` 的 `build_prompt`，与参考实现同口径）。
//! 工具调用是「注入式文本协议」且上游有产品级人格保护（陌生工具会被拒），
//! 参考实现标注为 best-effort —— 本模块第一版**不做**工具注入，文本直通。
//!
//! ── 与网关那份的差别（迁移时改的）───────────────────────────
//! | 网关 | 插件 |
//! |---|---|
//! | `ProviderAdapter` + `AccountStore` | `Provider` + `PluginCtx` |
//! | `ForwardOutcome::Stream/Completion` | `ConversationSink`（head + chunk） |
//! | `RequestTelemetry` 抓包 | **不迁**（插件的出站抓包由网关侧记） |
//! | `engine.rs` 的 DLL 签名参数体 | 退化成「最简表单」（见 `engine` 模块头） |
//! | 账号面（`api::accounts` 的 kuku 分支） | `accounts.rs`（`normalize_account`） |
//!
//! ── 模型 ──────────────────────────────────────────────────
//! 对话的模型参数 `model_name` 来自 `/wenchain/genflowpro/model_list` 的响应体
//! （kuku2api 用静态快照；本模块提供静态兜底 + 远程刷新，见 `models.rs`）。
//! 未知模型名上游会静默降级，因此本模块显式回退 `auto`（与参考实现同一取舍）。
//!
//! ── 余额 / 签到 ────────────────────────────────────────────
//! 余额：`GET /bizapi/gfpro/getgfvipremain` → `data.list[0].totalPoint`。
//! 签到：「免费领积分」活动的每日任务领取（`freepoint/homenew` 面板 +
//! `freepoint/taskComplete`，任务 `LOGIN` / `CHAT`），见 `checkin.rs`。
//! 业务会话前提（换发 genflowpro STOKEN）见 `engine.rs`。
//!
//! ── 刷新 / 登录形态 ────────────────────────────────────────
//! BDUSS 是百度通行证登录态 Cookie，**没有刷新接口**（`refresh = false`），
//! 过期只能重新登录 / 重新导入。添加路径三条（见 `accounts.rs`）：
//! 「粘贴 Cookie」（Cookie 头 / Cookie 编辑器 JSON / JSON 对象）、
//! 「导入本机客户端登录态」（`NormalizeSource::Desktop`）、
//! 「文件导入」（`NormalizeSource::FileBase64`，Cookies SQLite）。
//! 网页登录是**回调型**（`poll_interval_ms = 0`）：打开主站，壳侧从 WebView
//! 的 Cookie 存储读到 BDUSS 后把整串 Cookie 当 `code` 交回。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本目录零 unwrap/expect/panic，不持任何锁跨 await。

mod accounts;
mod balance;
mod chat;
mod checkin;
mod credentials;
mod engine;
mod http;
mod login;
mod models;
mod session;

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
/// 实测记录来自 kuku2api（网页形态），桌面形态只有 freepoint 的实测
/// （zhengwuji-workbuddy）。两组接口各认各的形态，照抄各自的实测值。
/// 余额接口（`/bizapi/gfpro/getgfvipremain`）**不带任何参数**（客户端
/// `http.get` 的 params 为空，见 `balance.rs` 模块头），这里没有它的常量。
pub const WEB_QUERY: &str = "clienttype=400&app_id=123971023&web=1&channel=chunlei&version=1.4.4";

/// 出站 User-Agent（kuku2api 实测可用；上游对 UA 敏感，不能用 reqwest 默认值）
pub const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 ",
    "(KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36"
);

/// 默认模型（上游自动选路；未知模型名显式回退它，避免上游静默降级）
pub const DEFAULT_MODEL: &str = "auto";

/// 思考模式默认档（参考实现固定 3；本模块不接思考等级绑定 ——
/// 上游 `think_mode` 的档位语义没有实测证据，按 trait 契约「没有证据就不注入」
/// 保持默认实现 `Skip`）
pub const DEFAULT_THINK_MODE: i64 = 3;

/// 本家 provider id（账号记录与日志都用它）
pub fn kuku_id() -> &'static str {
    "kuku"
}

/// 把一组头转成协议里的 `BTreeMap`（小写键）。
pub(super) fn header_map(headers: &[(String, String)]) -> std::collections::BTreeMap<String, String> {
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
                // 主站登录页（壳侧从 WebView Cookie 存储取回登录态）
                web_login: true,
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                // 没有环境变量旁路，也没有「无账号也能转发」的默认登录态
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: true,
                // 本家是否认「默认模型」概念（协议新增位，见 `Capabilities`）
                supports_default_model: false,
                // 本家有没有环境变量 / 默认登录态旁路（协议新增位）
                env_credentials: false,
            })
            .with_login(vec![
                proto::LoginMethod::Browser {
                    button: "网页登录".to_string(),
                    hint: Some(
                        "打开库库 AI 网页版，登录成功后自动取回登录态".to_string(),
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
                    hint: Some("读取库库 AI 桌面客户端已登录的 Cookies".to_string()),
                },
            ])
            .with_models(models::list())
            // 账号行：`usage` —— 余额/积分查询（`GET /bizapi/gfpro/getgfvipremain`
            // 的 `totalPoint`，见 `balance`）。
            //   `edition: false` —— 单一入口（粘贴 Cookie / 导入本机登录态 /
            //     网页登录），没有国际版。
            //   `identifier: uid` —— 账号标识存 uid（userreport 的 uk）。
            //   `expiry` 空、**不申报 `long_lived`** —— BDUSS 是登录态 Cookie，
            //     没有可读的到期时刻；但不能写成「长期有效」：上游随时可能让它
            //     失效（参考实现的故障排查里「BDUSS invalid」是最常见的一条），
            //     而且本家没有续期接口，失效只能重登。这一列对它只是「没有可
            //     显示的事实」。
            .with_account_features(proto::AccountFeatures {
                usage: true,
                identifier: vec!["uid".to_string()],
                ..Default::default()
            })
        .with_checkin(proto::CheckinInfo {
            description: Some("「免费领积分」的每日任务 · 逐个领取".to_string()),
            modes: vec![proto::CheckinMode::Checkin],
            ..Default::default()
        })
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
        Box::pin(async move { Ok(models::refresh(ctx, account, force).await) })
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

    /// 每日签到：freepoint 的 `LOGIN` / `CHAT` 两个任务。
    fn checkin<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(async move {
            let value = checkin::claim_daily_checkin(ctx, account)
                .await
                .map_err(GatewayError::into_message)?;
            Ok(checkin_report(&value))
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    fn normalize_account<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(accounts::normalize(ctx, input))
    }

    fn account_view(&self, secret: &Value) -> Result<Value, String> {
        Ok(accounts::account_view(secret))
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(accounts::identity_of(secret))
    }

    // ── 登录（回调型）──────────────────────────────────────────

    /// 发起登录：打开主站，壳侧登录完成后把 Cookie 当 `code` 交回。
    ///
    /// ── 为什么 `poll_interval_ms = 0` ──────────────────────────
    /// 百度通行证登录成功后的跳转**不携带授权码**（凭证在 `.baidu.com` 域的
    /// Cookie 里，跨域跳转不会带过去），因此没有可轮询的上游端点。网关据此
    /// 走「打开窗口 → 等回调」那条路，回调内容由壳侧从 WebView 的 Cookie
    /// 存储读出（见 `login.rs` 模块头）。
    fn login_start<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _method: &'a str,
        _payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async move { Ok(login::login_start()) })
    }

    fn login_callback<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        callback: proto::LoginCallback,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move {
            accounts::complete_login(&callback.code)
                .await
                .map_err(GatewayError::into_message)
        })
    }
}

/// `{success, msg}` → `CheckinReport`。
///
/// ── 为什么要翻译（而不是直接回 Value）──────────────────────
/// 插件协议的 `checkin.claim` 返回的是**六档 outcome**（`Claimed` / `Done` /
/// `Ineligible` / `Inactive` / `Captcha` / `Failed`），网关据此决定「今天的
/// 记录写成已领 / 不适用 / 需人工」。kuku 的 `{success, msg}` 只有两档，
/// 所以：`success=true` → `Claimed`（新领到或今天已领都算成功，网关的记录
/// 文案两者都写「已领」）；`success=false` → `Failed{message}`。
///
/// 「今天已经领过」在 kuku 的返回里是 `success=true` + 文案带「已领取」——
/// 归 `Claimed` 而不是 `Done` 是有意的：网关对 `Done` 的处置与 `Claimed`
/// 相同（都记「已领」），但 `Claimed` 的文案会带上游原话，用户能看到
/// 「今日已领取」这种更准确的说明。
fn checkin_report(value: &Value) -> proto::CheckinReport {
    let success = value
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let message = value
        .get("msg")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if success {
        proto::CheckinReport {
            outcome: proto::CheckinOutcome::Claimed,
            credit: None,
            streak: None,
            message: Some(if message.is_empty() {
                "签到成功".to_string()
            } else {
                message
            }),
        }
    } else {
        proto::CheckinReport {
            outcome: proto::CheckinOutcome::Failed,
            credit: None,
            streak: None,
            message: Some(if message.is_empty() {
                "签到失败（上游未给出原因）".to_string()
            } else {
                message
            }),
        }
    }
}
