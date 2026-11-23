//! Kiro（kiro.dev）插件实现：**会话式转发 + 浏览器登录 / 本机登录态 / API key**。
//!
//! ── 上游是什么 ──────────────────────────────────────────────
//! Kiro 是 AWS 的 AI IDE 订阅（Free / Pro / Pro+ / Power）。它没有 OpenAI
//! 兼容 API：模型走 Anthropic Messages 的语义，但传输是自家
//! `generateAssistantResponse` 的会话 JSON + AWS event stream 响应。
//!
//! ── 为什么 `stateful = true` ────────────────────────────────
//! 响应是 AWS event stream（二进制帧），网关的 `UpstreamKind` 是封闭枚举，
//! 没有这个协议。由插件自己读帧、翻成标准 chat SSE 是唯一不扩协议的路；
//! 代价是账号轮换之外的重试要在这里做（见 `chat.rs`）。
//!
//! ── 能力与「没有的东西」────────────────────────────────────
//! ```text
//!   有：对话（流式 / 工具调用 / 思考 / 图片）、凭证续期、在线模型目录、余额
//!   无：每日签到、领取类活动（上游确实没有这两类活动）
//! ```
//!
//! ── 与参考实现的差异 ───────────────────────────────────────
//! 参考实现把「Anthropic Messages 入站 + hosted web search」也做了；本网关
//! 的入站是 OpenAI Chat，没有 `web_search_20250305` 这类服务端工具，所以
//! 请求转换是 Chat → Kiro 会话（见 `request.rs`），不含 hosted web search。
//! 独立搜索经 `web-search.search` 管理动作提供给网关工具服务（见 `search.rs`），
//! 不改变聊天协议。其余协议知识（地址、头、事件名、错误文案）逐条对齐。

pub mod accounts;
pub mod chat;
pub mod credentials;
pub mod endpoints;
pub mod errors;
pub mod events;
pub mod eventstream;
pub mod image;
pub mod local;
pub mod login;
mod login_callback;
mod login_methods;
mod login_oauth;
pub mod models;
pub mod refresh;
pub mod request;
mod search;
pub mod usage;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ConversationSink, PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::Value;

/// provider id（`pack.json` 里也要写同一个）。
pub const PROVIDER_ID: &str = "kiro";

/// Kiro 适配器。
pub struct KiroProvider;

impl Provider for KiroProvider {
    fn meta(&self) -> ProviderMeta {
        let mut meta = ProviderMeta::new(PROVIDER_ID, "Kiro")
            .with_capabilities(proto::Capabilities {
                chat: true,
                // 响应是 AWS event stream，网关没有这条翻译机
                stateful: true,
                // OAuth 凭证有 refresh；API key 不续期（token_expiring 恒 false）
                refresh: true,
                model_refresh: true,
                usage: true,
                // 上游没有每日签到，也没有可领取活动
                checkin: false,
                claims: false,
                // 浏览器登录（Kiro 门户 / AWS Builder ID / IAM Identity Center）
                web_login: true,
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: true,
                // 模型名 `auto` 是 Kiro 自己的路由模型
                supports_default_model: true,
                env_credentials: false,
            })
            .with_login(vec![
                proto::LoginMethod::Browser {
                    button: "Kiro 网页登录".into(),
                    hint: Some(login_methods::INSTRUCTIONS.into()),
                },
                proto::LoginMethod::Desktop {
                    button: "导入 kiro-cli / Kiro IDE 登录态".into(),
                    hint: Some(
                        "直接使用本机 kiro-cli 或 Kiro IDE 已登录的账号（每次请求实时读取，\
                         续期后也会写回原处）。"
                            .into(),
                    ),
                },
                proto::LoginMethod::Token {
                    // 网关对手填登录方式的入口是「手动凭证 JSON」，所以这里
                    // 把 JSON 形态直接写进标签与占位符，用户不用猜字段名。
                    label: "Kiro API key（JSON：{\"apiKey\":\"ksk_…\"}）".into(),
                    placeholder: "{\"apiKey\":\"ksk_…\"}".into(),
                    extra_fields: vec![],
                },
            ])
            .with_models(models::fallback_models())
            .with_account_features(proto::AccountFeatures {
                usage: true,
                // 余额格点开是逐窗口明细（月度额度 / Free trial / Bonus credits）；
                // 明细行见 usage.rs 的 `credits`。
                credit_detail: true,
                credit_expiry: false,
                usage_detail: false,
                edition: false,
                identifier: vec!["accountId".into(), "uid".into()],
                expiry: vec!["expiresAt".into()],
                long_lived: false,
                claim: false,
                plan_channel: false,
                email_as_name: true,
                concurrency_default: None,
                default_limiters: None,
            });
        meta.order = 31;
        meta
    }

    fn normalize_account<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move { accounts::normalize(ctx, input).await })
    }

    fn account_view(&self, secret: &Value) -> Result<Value, String> {
        accounts::public(secret)
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(accounts::identity(secret))
    }

    // ── 凭证 ──────────────────────────────────────────────────

    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let (_, credentials) = refresh::ensure(ctx, account).await?;
            Ok(credentials.access)
        })
    }

    fn refresh_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let (_, credentials) = refresh::force_refresh(ctx, account).await?;
            Ok(credentials.access)
        })
    }

    fn token_expiring<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, bool> {
        Box::pin(async move {
            let stored = ctx
                .store_get(PROVIDER_ID, account)
                .await
                .map_err(|error| format!("读取 Kiro 账号失败：{error}"))?;
            let credentials = credentials::Credentials::from_secret(&stored.secret);
            if credentials.is_api_key() {
                return Ok(false);
            }
            if credentials.is_local() {
                let Some(live) = refresh::read_local(&credentials.source).await else {
                    return Ok(false);
                };
                return Ok(!live.fresh(endpoints::LEAD_MS));
            }
            Ok(!credentials.fresh(endpoints::LEAD_MS))
        })
    }

    // ── 目录 ──────────────────────────────────────────────────

    fn models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async move {
            let mut account_id = account.to_string();
            if account_id.is_empty() {
                if let Ok(stored) = ctx.store_get(PROVIDER_ID, "").await {
                    account_id = stored.id;
                }
            }
            Ok(models::list(&account_id))
        })
    }

    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            match models::refresh(ctx, account, force).await {
                Ok((outcome, _)) => Ok(outcome),
                Err(message) => Ok(proto::RefreshOutcome::failed(message)),
            }
        })
    }

    // ── 余额 ──────────────────────────────────────────────────

    fn usage<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::UsageReport> {
        Box::pin(async move { usage::query(ctx, account).await })
    }

    // ── 转发 ──────────────────────────────────────────────────

    fn conversation<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        request: proto::ConversationRequest,
        sink: ConversationSink<'a>,
    ) -> SendFuture<'a, ()> {
        Box::pin(chat::run(ctx, request, sink))
    }

    /// 动态登录方式（网关的「添加账号」弹窗按它渲染表单）。
    ///
    /// ── 为什么必须实现 ──────────────────────────────────────
    /// 网关优先问 `auth.methods`；不实现时整块降级成「手动凭证 JSON」，
    /// 用户看不到「读取本机登录态」这类一键入口（界面上还会多一个
    /// 「重新读取登录方式」按钮）。
    fn admin_action<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        action: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            match action {
                crate::native_search::CAPABILITIES | crate::native_search::SEARCH => {
                    search::dispatch(ctx, account, action, payload).await
                }
                "auth.methods" => Ok(login_methods::methods()),
                other => Err(agent2api_provider_sdk::unsupported(format!(
                    "Kiro 不支持管理动作「{other}」"
                ))),
            }
        })
    }

    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        errors::classify(status, body)
    }

    /// 思考档位：写进 `reasoning_effort`，由 `request.rs` 翻成 Kiro 的预算。
    fn reasoning_patch(&self, level: &str, _model: &str, body: &Value) -> proto::ReasoningPatch {
        if !body.get("reasoning_effort").is_none_or(Value::is_null) {
            return proto::ReasoningPatch::Skip {
                reason: "客户端已指定思考档位".into(),
            };
        }
        if matches!(level, "low" | "medium" | "high" | "xhigh" | "max") {
            proto::ReasoningPatch::Set {
                field: "reasoning_effort".into(),
                value: Value::String(level.into()),
            }
        } else {
            proto::ReasoningPatch::Skip {
                reason: "Kiro 不支持该思考档位".into(),
            }
        }
    }

    // ── 登录 ──────────────────────────────────────────────────

    fn login_start<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        method: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async move {
            if method != "browser" {
                return Err(
                    "Kiro 的登录请用「Kiro 网页登录」或「导入 kiro-cli / Kiro IDE 登录态」".into(),
                );
            }
            let index = payload.get("methodIndex").and_then(Value::as_u64);
            match index {
                // 1 = 本机登录态（kiro-cli / Kiro IDE），点一下直接完成
                Some(1) => login::start_local().await,
                // 2 = API key，走「粘贴凭证」而不是登录流程
                Some(2) => Err("Kiro API key 请用「粘贴 API key」方式添加".to_string()),
                Some(0) | None => {
                    let proxy = payload.get("proxy").filter(|value| !value.is_null())
                        .map(|value| serde_json::from_value::<proto::WireProxy>(value.clone()))
                        .transpose().map_err(|_| "Kiro 登录代理配置无效")?;
                    let inputs = payload.get("inputs").unwrap_or(&Value::Null);
                    login::start(proxy.as_ref(), inputs).await
                }
                Some(_) => Err("请选择有效的 Kiro 登录方式".into()),
            }
        })
    }

    fn login_poll<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move { Ok(login::poll(flow).await) })
    }

    fn login_callback_result<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        callback: proto::LoginCallback,
    ) -> SendFuture<'a, agent2api_provider_sdk::LoginCallbackResult> {
        Box::pin(async move { Ok(login::callback(callback).await) })
    }

    fn login_cancel<'a>(&'a self, _ctx: &'a PluginCtx, flow: &'a str) -> SendFuture<'a, ()> {
        Box::pin(async move {
            login::cancel(flow);
            Ok(())
        })
    }
}
