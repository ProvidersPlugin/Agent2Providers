//! Loomy（讯飞）插件实现：账号管理（手机号验证码登录）**加推理转发**。
//!
//! ── 上游长什么样（依据：Loomy 桌面端 `app.asar`）──────────
//! 三套平面（细节见各子模块的模块头）：
//!
//! ```text
//!   账号 CAccount     POST https://account.xfinfr.com/login/phone/{sendMsgCode,checkCode}
//!                     HMAC-SHA1 签名头（客户端内置 AccessKey 对，见 sign.rs）
//!   集成网关（积分）  https://loomyad.xunfei.cn        header: token: <session>
//!   模型网关          {集成网关}/api/v1                 OpenAI 兼容，token + Bearer 双头
//! ```
//!
//! 本家**无状态**（一次 HTTP 请求 = 一次回答，OpenAI 协议），与 AutoClaw 同构：
//! `stateful` 为 false，转发链只需 `build_chat`。
//!
//! ── 三处与别家不同的取舍（都写在各自模块头，这里给索引）──────
//!   1. **没有续期**（`refresh = false`）：session 14 天，上游没有 refresh 接口，
//!      过期（`020002` / `100002`）只能重新短信登录 —— 见 `credentials.rs`；
//!   2. **没有桌面端导入**：登录态不在可读文件里；
//!   3. **没有内置兜底模型清单**：官方 imodel 网关 `/models` 拉不到就如实报错，
//!      不编模型名 —— 见 `models.rs`。
//!
//! ── 思考等级 ────────────────────────────────────────────────
//! 客户端给模型挂 OpenCode variant（low/medium/high）→ AI SDK 发
//! `reasoning_effort`；这里把通用档位折成这三档注入（客户端显式指定时让位）。
//! 只有 spark-x 是真思考模型，但上游对非思考模型忽略该字段是已知行为
//! （客户端对全部 OpenAI 兼容模型都挂了 variants），因此不做按模型的收窄。
//!
//! ── 每日「签到」是每日首次登录刷新积分 ──────────────────────
//! `POST {集成网关}/api/v1/points/first-login`（幂等，按天）。

pub mod balance;
pub mod checkin;
pub mod client;
pub mod credentials;
pub mod endpoints;
pub mod images;
mod image_generation;
pub mod login;
pub mod models;
pub mod onboarding;
pub mod sign;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ChatPlan, PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::Value;

/// provider id（落盘契约，不能改）。
pub const ID: &str = "loomy";

pub struct LoomyProvider;

impl Provider for LoomyProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(ID, "Loomy")
            .with_capabilities(proto::Capabilities {
                chat: true,
                // 一次 HTTP 请求 = 一次回答（OpenAI 协议）
                stateful: false,
                // 上游没有 refresh 接口，session 14 天到期只能重登
                refresh: false,
                model_refresh: true,
                // 双账户积分（永久 + 每日赠送）
                usage: true,
                // 每日首次登录刷新赠送积分
                checkin: true,
                // 新手任务（8 条一次性福利）
                claims: true,
                // 只有手机验证码登录，不开授权窗口
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
            .with_login(vec![proto::LoginMethod::Sms {
                hint: Some("Fill in the phone number, then the 6-digit SMS code".into()),
            }])
            // 本家没有内置兜底清单（见 `models.rs` 的模块头）
            .with_models(vec![])
            // 账号行：`usage` 对应 `balance` —— 余额是**双账户**（永久积分 +
            // 每日赠送积分，读 `GET /api/v2/points/records` 的摘要字段），
            // 界面上两张卡都列。
            //   `edition: false` —— 单一入口（手机验证码登录），没有地区概念。
            //   `expiry: []` —— **不申报有效期**。上游登录响应只有
            //     `{session, userid, phone}`，没有任何到期时刻；以前按「登录时刻
            //     + 14 天」估算一个值填进 `expiresAt`，结果账号用满两周就红着写
            //     「已过期」，而上游其实照常放行（实测：估算到期后仍能签到与转发）。
            //     估算值不是事实，不能当有效期展示。真正的失效判据只有一个 ——
            //     上游回 `020002` / `100002`，那时由网关写 `needsRelogin`
            //     （见 `mod.rs` 的 `classify` 与网关 `provider_loop`）。
            //   **不申报 `long_lived`**：session 确实会失效，只是失效时刻不可知。
            //   `credit_detail` —— 双账户各一行（永久积分 / 每日赠送积分），
            //     后者标 `daily` → 界面写「每日重置」而不是到期。
            .with_account_features(proto::AccountFeatures {
                usage: true,
                credit_detail: true,
                identifier: vec!["uid".to_string()],
                expiry: Vec::new(),
                ..Default::default()
            })
        .with_checkin(proto::CheckinInfo {
            description: Some("无独立签到接口 · 每天替账号打一次首次登录积分".to_string()),
            modes: vec![proto::CheckinMode::Checkin],
            onboarding: true,
            ..Default::default()
        })
    }

    // ── 数据面 ────────────────────────────────────────────────

    /// 构造 `POST {模型网关}/chat/completions`（OpenAI 协议，body 原样透传）。
    ///
    /// 鉴权是**双头**（客户端 `fetchModelsFromProvider` 的 authMode: 'token' 同款）：
    /// `token: <session>` 与 `Authorization: Bearer <session>` 都发 —— 上游两个
    /// 网关（模型 / 积分）对头名的偏好不同，双写是对两边都安全的做法。
    fn build_chat<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ChatRequest,
    ) -> SendFuture<'a, ChatPlan> {
        Box::pin(async move {
            let credentials = credentials::snapshot(ctx, &req.account).await?;
            let token = credentials.session;
            let headers: Vec<(String, String)> = vec![
                ("token".to_string(), token.clone()),
                ("Authorization".to_string(), format!("Bearer {token}")),
                ("Accept".to_string(), "text/event-stream".to_string()),
            ];
            // 图片格式适配（见 `images` 模块头）：只有**上游没有视觉通路**的模型
            // 才转 `<image_base64>` 标记；目录声明支持图片输入的模型保持标准
            // image_url —— 对它来说标记反而会把 base64 当纯文本塞进 prompt。
            //
            // 网关在调本方法**之前**已经把 `body.model` 改写成本家要收的名字
            // （见网关 `upstream::payload::send_body`），所以这里读的就是发送名。
            let requested_model = req
                .body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let (payload, converted) = images::adapt_images(&req.body, &requested_model);
            if converted > 0 {
                agent2api_provider_sdk::log_info!(
                    "[Loomy] 模型 {requested_model} 无视觉通路：已把 {converted} 个图片块转成 <image_base64> 标记"
                );
            }
            Ok(ChatPlan::chat(
                format!("{}/chat/completions", endpoints::model_base_url()),
                headers,
                payload,
            ))
        })
    }

    fn supports_image_generation(&self) -> bool {
        true
    }

    fn supports_image_editing(&self) -> bool {
        true
    }

    fn build_image_edit<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: agent2api_provider_sdk::images::ImageRequest,
    ) -> SendFuture<'a, agent2api_provider_sdk::images::ImagePlan> {
        Box::pin(image_generation::build_edit(ctx, req))
    }

    fn build_image<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: agent2api_provider_sdk::images::ImageRequest,
    ) -> SendFuture<'a, agent2api_provider_sdk::images::ImagePlan> {
        Box::pin(image_generation::build(ctx, req))
    }

    /// 上游错误分类：
    ///   - HTTP 401 / 403 → `TokenExpired`；
    ///   - 业务码 `020002` / `100002`（HTTP 200 或非 2xx 里的 body）→ `TokenExpired`；
    ///   - HTTP 429 → `QuotaLimited`（上游不给结构化恢复时间）；
    ///   - 其余 → 内容拦截分类（审核文案）或原样透传。
    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        let raw = body
            .get("message")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误");
        let message = format!("上游返回 {status}: {raw}");
        let body_code = body
            .get("code")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if status == 401 || status == 403 || client::is_auth_error_code(body_code) {
            return proto::ErrorClass::TokenExpired { message };
        }
        if status == 429 {
            return proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: body.get("code").and_then(Value::as_i64),
                status,
            };
        }
        content_or_fatal(
            status,
            body,
            message,
            body.get("code").and_then(Value::as_i64),
        )
    }

    /// 思考等级绑定 → `reasoning_effort`（low / medium / high）。
    ///
    /// 通用 6 档折成三档：`minimal|low → low`、`medium|high → medium`、
    /// `xhigh|max → high`。客户端显式指定时让位；`off` / `none` 不注入；
    /// 表外自定义等级不注入（上游对未知档位要么忽略要么 400）。
    fn reasoning_patch(&self, level: &str, _model: &str, body: &Value) -> proto::ReasoningPatch {
        if crate::reasoning::is_thinking_off(level) {
            return proto::ReasoningPatch::Skip {
                reason: "Loomy 没有「关闭思考」的可靠表达，跳过注入".to_string(),
            };
        }
        if crate::reasoning::read_client_level(body).is_some() {
            return proto::ReasoningPatch::Skip {
                reason: "客户端请求体里已指定思考等级，绑定让位".to_string(),
            };
        }
        let Some(rank) = crate::reasoning::effort_rank(level) else {
            return proto::ReasoningPatch::Skip {
                reason: "该等级不在通用档位表内，Loomy 不注入未知档位".to_string(),
            };
        };
        let effort = match rank {
            0 | 1 => "low",
            2 | 3 => "medium",
            _ => "high",
        };
        proto::ReasoningPatch::Set {
            field: "reasoning_effort".to_string(),
            value: Value::String(effort.to_string()),
        }
    }

    // ── 凭证 ──────────────────────────────────────────────────

    /// 取可用 session。本家**没有续期**，也**不预测失效**：这里只做「记录里
    /// 有没有 session」的检查。服务端拒了（`020002` / `100002` / 401）才是真的
    /// 失效，那时编排层归成 `TokenExpired` 并给账号落 `needsRelogin`。
    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let credentials = credentials::snapshot(ctx, account).await?;
            Ok(credentials.session)
        })
    }

    /// 没有续期手段（上游没有 refresh 接口）——维护任务不问本家。
    ///
    /// 恒 `false`：本家**没有可预测的到期时刻**（上游不返回，估算值会误杀
    /// 仍然可用的账号）。失效只由转发链路上游的拒绝来确认，见 `ensure_token`。
    fn token_expiring<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, bool> {
        Box::pin(async { Ok(false) })
    }

    // ── 目录 / 余额 / 签到 ────────────────────────────────────

    fn models<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async { Ok(models::catalog()) })
    }

    /// 刷模型目录：用指定账号（空串 = 库里第一个可用账号）的 session。
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
            let credentials = match ctx.store_get(ID, account).await {
                Ok(found) => match credentials::from_secret(&found) {
                    Ok(credentials) => credentials,
                    Err(reason) => return Ok(proto::RefreshOutcome::failed(reason)),
                },
                Err(_) if account.is_empty() => {
                    agent2api_provider_sdk::log_info!(
                        "[Loomy] 模型目录刷新跳过：尚未添加账号"
                    );
                    return Ok(proto::RefreshOutcome::unchanged());
                }
                Err(error) => {
                    return Ok(proto::RefreshOutcome::failed(format!(
                        "读取 Loomy 账号失败：{error}"
                    )))
                }
            };
            let client = crate::http::control_client(None, &ctx.env.proxy);
            Ok(models::refresh(&client, &credentials.session, force).await)
        })
    }

    fn quota_sources(&self, report: &Value) -> Vec<Value> {
        crate::quota::sources(&self.meta().id, report)
    }

    fn usage<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::UsageReport> {
        Box::pin(balance::query_usage(ctx, account))
    }

    /// 每日「签到」= 调一次每日首次登录刷新接口（幂等，按天）。
    fn checkin<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(checkin::claim_daily_login(ctx, account))
    }

    /// 新手任务：`claim` 传任务 key，空串 = 一键领取全部未完成项。
    fn claim<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        claim: &'a str,
    ) -> SendFuture<'a, Value> {
        Box::pin(onboarding::claim(ctx, account, claim))
    }

    /// 新手任务状态（只读）。`refresh = true` 是用户手动点了「查询任务」
    /// （跳过结算记忆实查一次，与内置版的 `?refresh=1` 同义）。
    fn claim_status<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        refresh: bool,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            onboarding::get_tasks(ctx, account, refresh)
                .await
                .map_err(|(_, message)| message)
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    fn normalize_account<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(credentials::normalize(ctx, input))
    }

    fn account_view(&self, secret: &Value) -> Result<proto::AccountView, String> {
        credentials::account_view(secret)
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(credentials::identity_of(secret))
    }

    /// 管理动作：短信登录的两步（**同步形态**，与 `login.start` 那条异步流程并列）。
    ///
    /// ```text
    ///   login.sms-send    {phone}                → {msgid}
    ///   login.sms-verify  {phone, code, msgid}   → NormalizedAccount（网关落库）
    /// ```
    ///
    /// ── 为什么不用 `login.start` / `login.callback` ─────────────
    /// 那两条是**浏览器登录任务**的框架（开窗口、轮询、落临时 handle），而本家
    /// 没有网页授权码 —— 官方客户端唯一的自助入口就是手机号 + 验证码，两次
    /// 同步调用就结束了。硬套那个框架会让网关为一次短信建一个后台任务、
    /// 还要把 msgid 编进 flow 再拆出来（插件侧的 `login_start` 正是这么做的，
    /// 那是为了**协议一致性**；面板上这条路径更直接）。
    ///
    /// 两条路共用同一份 `login::send_code` / `login::login_with_code`，
    /// 不存在「两套实现」。
    fn admin_action<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        _account: &'a str,
        action: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            match action {
                "login.sms-send" => {
                    let phone = payload
                        .get("phone")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    login::send_code(&ctx.env.proxy, phone).await
                }
                "login.sms-verify" => {
                    let phone = payload
                        .get("phone")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let code = payload
                        .get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let msgid = payload
                        .get("msgid")
                        .and_then(Value::as_str)
                        .filter(|value| !value.trim().is_empty());
                    let account =
                        login::login_with_code(&ctx.env.proxy, phone, code, msgid, None).await?;
                    serde_json::to_value(account)
                        .map_err(|error| format!("登录结果无法序列化: {error}"))
                }
                other => Err(format!(
                    "Loomy 不认识管理动作「{other}」（可用：login.sms-send / login.sms-verify）"
                )),
            }
        })
    }

    // ── 登录（手机验证码，两步）───────────────────────────────

    /// `method = "sms"`，`payload = {phone}`：发码，把「msgid + 手机号」编成
    /// `flow` 回给网关。
    ///
    /// ── 为什么用 `flow` 携带而不是让插件记状态 ──────────────────
    /// 一次登录可以跨多次 HTTP 请求、也可以被用户放弃。插件进程重启后那份
    /// 内存状态就没了（而用户的浏览器窗口还开着）。把两段信息编进 `flow`
    /// 让这条流程**无状态**：网关原样带回来，插件就能继续。
    ///
    /// `url` 为空 + `poll_interval_ms = 0`：这是「用户还要填一个码」的流程，
    /// 网关不该开窗口、也不该轮询，等用户提交后走 `login.callback`。
    fn login_start<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        method: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async move {
            if method != "sms" && method != "token" {
                return Err(format!("Loomy 不支持「{method}」登录方式"));
            }
            let phone = payload.get("phone").and_then(Value::as_str).unwrap_or("");
            let normalized = login::normalize_phone(phone)?;
            let result = login::send_code(&ctx.env.proxy, &normalized).await?;
            let msgid = result
                .get("msgid")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if msgid.is_empty() {
                return Err("上游没有返回 msgid".to_string());
            }
            Ok(proto::LoginStart {
                // `msgid|phone`：回调时拆开即可，插件不需要记住中间态
                flow: format!("{msgid}|{normalized}"),
                url: String::new(),
                instructions: "Fill in the 6-digit code sent to your phone".to_string(),
                poll_interval_ms: 0,
                external_browser: false,
                allowed_hosts: Vec::new(),
            })
        })
    }

    /// 用户提交验证码：`flow` 是 `login.start` 给的那串，`code` 是短信码。
    fn login_callback<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        callback: proto::LoginCallback,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move {
            let (msgid, phone) = match callback.flow.split_once('|') {
                Some((msgid, phone)) => (msgid.trim(), phone.trim()),
                None => ("", ""),
            };
            if msgid.is_empty() || phone.is_empty() {
                return Ok(proto::LoginPoll::Failed {
                    message: "登录会话已失效：请重新获取验证码".to_string(),
                });
            }
            // 网关给的 `code` 优先；有些客户端把整条回调串塞进来，取最后一个
            // `code=` 参数兜底（与内置版的入口口径一致）。
            let mut code = callback.code.trim().to_string();
            if code.is_empty() {
                code = query_param(&callback.url, "code");
            }
            match login::login_with_code(&ctx.env.proxy, phone, &code, Some(msgid), None).await {
                Ok(account) => Ok(proto::LoginPoll::Done { account }),
                Err(message) => Ok(proto::LoginPoll::Failed { message }),
            }
        })
    }
}

/// 从 URL 查询串里取一个参数（没有则空串）。
fn query_param(url: &str, key: &str) -> String {
    let Some(query) = url.split_once('?').map(|(_, rest)| rest) else {
        return String::new();
    };
    for pair in query.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if name == key {
            return value.trim().to_string();
        }
    }
    String::new()
}

/// 内容策略拦截的文案特征（全部小写；匹配前把上游文案转小写）
const CONTENT_BLOCK_PATTERNS: &[&str] = &[
    "blocked by security policy",
    "unapproved channel",
    "illegal api invocation",
];

/// 内容策略拦截的提示（与网关 `content_block::CONTENT_BLOCK_HINT` 逐字一致）
const CONTENT_BLOCK_HINT: &str = "；内容被上游内容策略拒绝（这类拦截按逐字匹配，不是账号问题）：\
若为客户端 system 模板误报，可在设置页「通用 → 系统提示词」切到「替换」模式；\
若为对话内容本身触发审核，请调整后重试";

/// 命中内容拦截文案 → `ContentBlocked`（并补提示），否则 → `Fatal`。
fn content_or_fatal(
    status: u16,
    body: &Value,
    message: String,
    code: Option<i64>,
) -> proto::ErrorClass {
    let matched = status >= 400
        && body
            .get("message")
            .and_then(Value::as_str)
            .map(|text| {
                let lowered = text.to_lowercase();
                CONTENT_BLOCK_PATTERNS
                    .iter()
                    .any(|pattern| lowered.contains(pattern))
            })
            .unwrap_or(false);
    if matched {
        proto::ErrorClass::ContentBlocked {
            status,
            message: format!("{message}{CONTENT_BLOCK_HINT}"),
            upstream_code: code,
        }
    } else {
        proto::ErrorClass::Fatal {
            status,
            message,
            upstream_code: code,
        }
    }
}
