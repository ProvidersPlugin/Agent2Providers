//! MonkeyCode 提供商（两个地区共用一套实现，按 `Region` 参数化）。
//!
//! ── 上游长什么样（依据 `Acankao/MonkeyCodeReverseEngineer`）──────
//! 长亭科技（chaitin）的开源 AI 开发平台，单体 Web 应用，网页与 API 同域：
//!
//! ```text
//!   站点      国内 https://monkeycode-ai.com   国际 https://monkeycode-ai.net
//!   认证      GET  /api/v1/users/status        Cookie: monkeycode_ai_session=…
//!   目录      GET  /api/v1/users/models        → {code:0,data:{models:[…]}}
//!   任务      POST /api/v1/users/tasks         （建任务；见 task.rs）
//!   任务流    WS   /api/v1/users/tasks/stream  （ACP 事件；见 stream.rs）
//! ```
//!
//! ── 为什么是两家 provider（国内 .com / 国际 .net）───────────────
//! 同一套协议、两个站点，与 AutoClaw / Qoder / ZCode 的两地区同款建模：
//! 地区是**provider 身份**而不是账号属性（`region::Region` 是地区 → 名称 /
//! 域名的唯一事实来源）。
//!
//! ── 会话转发为什么是有状态的（`stateful`）─────────────────────
//! 一次对话 = 建任务 + 挂 WebSocket 任务流 + 消费 ACP 事件，不是「一次 HTTP
//! 请求一次回答」，单请求构造容纳不了这条链。转发实现按职责拆成几个文件：
//!   `task.rs`      输入侧翻译（messages → prompt）与建任务（POST /users/tasks）
//!   `stream.rs`    WS 连接、起始消息（auto-approve / user-input）、心跳、超时、
//!                  终态与错误帧（四个出口都会给下游一个收尾，不会挂死）
//!   `translate.rs` 下行消息与 ACP 事件 → OpenAI chunk、自动回复 Agent 提问
//!   `chat.rs`      装配入口（`conversation` 转调）与流式 / 非流式分流
//!
//! ── 与网关那份的差别（迁移时改的）───────────────────────────
//! | 网关 | 插件 |
//! |---|---|
//! | `ProviderAdapter` + `AccountStore` | `Provider` + `PluginCtx` |
//! | `ForwardOutcome::Stream/Completion` | `ConversationSink`（head + chunk） |
//! | `RequestTelemetry` 抓包 / 用量上报 | **不迁**（插件的出站抓包与用量记账由网关侧记） |
//! | `catalog_cache` 落盘缓存 | 只留内存（网关重启后第一轮刷新真打一次上游） |
//! | 账号面（`api::accounts` 的 monkeycode 分支） | `accounts.rs` |
//!
//! ── 本家没有的东西（如实声明，别照抄别家）──────────────────────
//!   - **没有续期**：session 30 天硬限制，上游无 refresh 接口 → `refresh: false`；
//!   - **没有网页登录**：登录要么带验证码、要么要 OAuth 窗口，插件只接
//!     「粘贴 session」这一条 → `web_login: false`；
//!   - **不做签到与余额**：没有实测可用的余额接口 → `usage: false`、`checkin: false`；
//!   - **任务流不挂账号代理**：tokio-tungstenite 没有代理支持，参考实现同样
//!     只给 REST 调用挂代理（见 `stream::connect` 的说明）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本目录零 unwrap/expect/panic。

mod accounts;
mod chat;
mod client;
mod credentials;
mod endpoints;
mod inflight;
mod login;
mod models;
mod region;
mod stream;
mod task;
mod translate;

use serde_json::Value;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ConversationSink, PluginCtx, Provider, ProviderMeta, SendFuture};

use crate::errors::GatewayError;

pub use region::Region;

/// 国内版实例（`monkeycode`）
pub static MONKEYCODE_CN: MonkeyCodeProvider = MonkeyCodeProvider { region: Region::Cn };
/// 国际版实例（`monkeycode-intl`）
pub static MONKEYCODE_INTL: MonkeyCodeProvider = MonkeyCodeProvider { region: Region::Intl };

/// MonkeyCode 提供商：**按地区参数化**（与 Qoder / AutoClaw 同款）。
///
/// `Clone` 是为了让 [`MONKEYCODE_CN`] / [`MONKEYCODE_INTL`] 两个静态实例
/// 能按值装进 `Vec<Box<dyn Provider>>`（`Region` 是 `Copy`，克隆是零成本的）。
#[derive(Clone, Copy)]
pub struct MonkeyCodeProvider {
    region: Region,
}

impl Provider for MonkeyCodeProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(
            self.region.provider_id(),
            format!("MonkeyCode {}", self.region.label()),
        )
        .with_capabilities(proto::Capabilities {
            chat: true,
            // 建任务 → WebSocket 任务流 → ACP 事件（见模块头）
            stateful: true,
            // session 30 天硬限制，上游无 refresh 接口
            refresh: false,
            // `GET /api/v1/users/models`
            model_refresh: true,
            // 没有实测可用的余额接口
            usage: false,
            // 没有签到
            checkin: false,
            claims: false,
            // 登录要验证码 / OAuth 窗口，插件只接「粘贴 session」
            web_login: false,
            sse_model_rewrite: false,
            sse_strip_newline_chunks: false,
            anonymous_default_session: false,
            system_proxy_when_unset: false,
            refresh_uses_account: true,
            // 本家是否认「默认模型」概念（协议新增位，见 `Capabilities`）
            supports_default_model: false,
            // 本家有没有环境变量 / 默认登录态旁路（协议新增位）
            env_credentials: false,
        })
        .with_login(vec![
            proto::LoginMethod::Token {
                label: "Session".to_string(),
                placeholder: "monkeycode_ai_session 的值（可整行粘贴 Cookie）".to_string(),
                extra_fields: vec![proto::LoginField {
                    key: "imageId".to_string(),
                    label: "image_id（可选）".to_string(),
                    placeholder: "留空则自动从历史任务发现；新账号可能需手动填".to_string(),
                    optional: true,
                    multiline: false,
                }],
            },
        ])
        .with_models(models::list(self.region))
        // 账号行：两个站点都要申报（漏一个那一家就掉进兜底）。
        //   `usage: false` —— 本家**确实没有余额查询**：适配器的 `supports_usage`
        //     保持默认 false，这是「上游没有这个接口」，不是「还没接」。
        //   `edition: false` —— 地区已经由 provider id 表达（两家、两个分组），
        //     再拼一次后缀会得到「MonkeyCode 国际版 国际版」。
        //   `identifier: userId` —— 公开形态里有 `userId`。
        //   `expiry: expiresAt` —— 公开形态给的是 `expiresAt`（session cookie 的
        //     30 天硬限制），**不是** `tokenExpiresAt`。本家没有续期协议，
        //     所以**不申报 `long_lived`**：凭证有固定寿命，只是过期后救不回来。
        .with_account_features(proto::AccountFeatures {
            identifier: vec!["userId".to_string()],
            expiry: vec!["expiresAt".to_string()],
            ..Default::default()
        })
    }

    /// 模型清单来自远程目录（进程内存缓存，按地区分格）。
    fn models<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async move {
            Ok(proto::ModelList {
                models: models::list(self.region),
                meta: proto::CatalogMeta {
                    remote: models::remote_refreshed(self.region),
                    refreshed_at: models::last_refreshed_at(self.region),
                },
            })
        })
    }

    /// 会话式转发入口：转调 `chat::run_chat`。
    fn conversation<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ConversationRequest,
        sink: ConversationSink<'a>,
    ) -> SendFuture<'a, ()> {
        Box::pin(chat::run_chat(ctx, self.region, req, sink))
    }

    /// 上游错误分类：
    ///   - HTTP 401 / 403 → `TokenExpired`（登录态失效，重新粘贴 session）；
    ///   - HTTP 402 → `Fatal`（额度不足：换账号也没用，用户得去官网处理）；
    ///   - HTTP 429 → `QuotaLimited`（上游不给结构化恢复时间）；
    ///   - 其余 → `Fatal`。
    ///
    /// ── 这条路径在本家事实上走不到（如实说明）──────────────────
    /// 本家是 `stateful`，所有上游错误都在 `task.rs` / `stream.rs` 里现场
    /// 归一到 `GatewayError`（状态码与文案已按建任务 / 任务流的码表翻译好）；
    /// `classify` 只服务单请求路径。保留这些分支是让契约完整 + 排障时一眼
    /// 看到码表的映射意图。
    fn classify(&self, status: u16, error_body: &Value) -> proto::ErrorClass {
        let upstream_message = error_body
            .get("message")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string);
        let hint = match status {
            402 => Some("（账户额度不足，请在 MonkeyCode 官网确认订阅 / 余额）"),
            _ => None,
        };
        let message = match (upstream_message, hint) {
            (Some(text), Some(hint)) => format!("上游返回 {status}: {text}{hint}"),
            (Some(text), None) => format!("上游返回 {status}: {text}"),
            (None, Some(hint)) => format!("上游返回 HTTP {status}{hint}"),
            (None, None) => format!("上游返回 HTTP {status}"),
        };
        if status == 401 || status == 403 {
            return proto::ErrorClass::TokenExpired { message };
        }
        if status == 429 {
            return proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: error_body.get("code").and_then(Value::as_i64),
                status,
            };
        }
        proto::ErrorClass::Fatal {
            status,
            message,
            upstream_code: error_body.get("code").and_then(Value::as_i64),
        }
    }

    /// 取可用 session。本家**没有续期**：只做「记录里有没有 session」的检查。
    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let record = record_of(ctx, self.region, account).await?;
            let credentials = credentials::from_record(record.as_ref())?;
            if credentials.expiring() {
                crate::logging::verbose(
                    "[MonkeyCode]",
                    "账号凭证已过期或临期（会话 30 天、无续期接口），如遇 401 请重新粘贴 session",
                );
            }
            Ok(credentials.session)
        })
    }

    /// 临期判定：取记录 → 凭证的 `expiring()`（登录时间 + 30 天，提前 1 天）
    fn token_expiring<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, bool> {
        Box::pin(async move {
            if account.is_empty() {
                return Ok(false);
            }
            let record = match record_of(ctx, self.region, account).await {
                Ok(record) => record,
                Err(_) => return Ok(false),
            };
            Ok(credentials::from_record(record.as_ref())
                .map(|credentials| credentials.expiring())
                .unwrap_or(false))
        })
    }

    /// 刷模型目录：用指定账号（空串 = 本地区组内第一个可用账号）的 session。
    ///
    /// 「一个账号都没有」不是失败 —— 那是「用户还没添加账号」的正常状态，
    /// 返回 `unchanged()`（脚本 / CI 不该看到一个红色失败）。
    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            let record = record_of(ctx, self.region, account).await?;
            if record.is_none() {
                if account.is_empty() {
                    crate::logging::verbose(
                        "[Models]",
                        &format!(
                            "MonkeyCode {} 模型目录刷新跳过：尚未添加账号",
                            self.region.label()
                        ),
                    );
                    return Ok(proto::RefreshOutcome::unchanged());
                }
                return Ok(proto::RefreshOutcome::failed(
                    "指定的账号不存在或不可用，请重新选择",
                ));
            }
            let credentials = match credentials::from_record(record.as_ref()) {
                Ok(credentials) => credentials,
                Err(error) => return Ok(proto::RefreshOutcome::failed(error.message)),
            };
            Ok(models::refresh(self.region, &credentials.session, force).await)
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    fn normalize_account<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(accounts::normalize(self.region, input))
    }

    fn account_view(&self, secret: &Value) -> Result<Value, String> {
        Ok(accounts::account_view(self.region, secret))
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(accounts::identity_of(secret))
    }
}

/// 读一条账号记录（`account` 为空串 = 本地区队首账号）。
///
/// ── 为什么空串要显式处理 ──────────────────────────────────
/// `store_get` 在 `account` 为空串时返回该 provider 的**队首**账号（网关的
/// 契约）。但「指定了 account_id 却查不到」与「没指定」必须分开：前者是
/// 错误（账号被删了），后者是正常路径（用队首）。这里把两者区分开。
async fn record_of(
    ctx: &PluginCtx,
    region: Region,
    account: &str,
) -> Result<Option<Value>, GatewayError> {
    let found = crate::store::record(ctx, region.provider_id(), account).await?;
    if found.is_null() {
        if account.is_empty() {
            return Ok(None);
        }
        return Err(GatewayError::with_status(
            401,
            format!(
                "MonkeyCode {} 账号 {account} 不存在（请重新添加）",
                region.label()
            ),
        ));
    }
    Ok(Some(found))
}
