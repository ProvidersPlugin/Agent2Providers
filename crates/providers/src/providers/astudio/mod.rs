//! AStudio（科大讯飞 Astron Studio 桌面端）插件实现。
//!
//! ── 上游是什么 ──────────────────────────────────────────────
//! 讯飞「星火 Astron Studio」桌面端登录后，把模型凭据与会话写进本机数据
//! 目录。模型调用走 OpenAI **Responses** 协议（`/model-gateway/v1/responses`），
//! 积分 / 会员 / 签到走 `agent.xfyun.cn` 的网页会话（Cookie）。
//!
//! ── 能力 ────────────────────────────────────────────────────
//! ```text
//!   有：对话（流式 / 工具 / 图片 / 思考）、在线模型目录、余额、每日登录积分
//!   无：凭证续期（凭据失效只能重新读取/粘贴）、领取类活动（上游没有接口）
//! ```
//!
//! ── 为什么 `stateful = true` ────────────────────────────────
//! 上游响应是 Responses 的 SSE 事件（`response.output_text.delta` 等），
//! 网关的 `UpstreamKind` 没有这一套。由插件自己读事件、翻成 chat 帧。
//!
//! ── 两种账号 ────────────────────────────────────────────────
//! 「读取桌面端登录态」拿到模型凭据 + 网页会话（推荐）；「粘贴
//! modelBearerToken」只保证模型调用，余额与签到需要另填网页会话字段。

pub mod accounts;
pub mod auth;
pub mod chat;
pub mod checkin;
pub mod client;
pub mod endpoints;
pub mod login;
pub mod login_page;
pub mod models;
pub mod responses;
pub mod usage;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ConversationSink, PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::Value;

/// provider id（`pack.json` 里也要写同一个）。
pub const PROVIDER_ID: &str = endpoints::PROVIDER_ID;

/// AStudio 适配器。
pub struct AStudioProvider;

impl Provider for AStudioProvider {
    fn meta(&self) -> ProviderMeta {
        let mut meta = ProviderMeta::new(PROVIDER_ID, "AStudio")
            .with_capabilities(proto::Capabilities {
                chat: true,
                stateful: true,
                // 没有 refresh 接口：凭据失效只能重新读取桌面端登录态或重新粘贴
                refresh: false,
                model_refresh: true,
                usage: true,
                checkin: true,
                claims: false,
                // 手机号 / 账号密码走本机登录页（oauth），粘贴 modelBearerToken 走 api
                web_login: true,
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: true,
                supports_default_model: true,
                env_credentials: false,
            })
            .with_login(vec![
                proto::LoginMethod::Token {
                    label: "粘贴 modelBearerToken（JSON：{\"modelBearerToken\":\"…\"}）".into(),
                    placeholder:
                        "{\"modelBearerToken\":\"…\",\"sessionToken\":\"…\",\"accountId\":\"…\"}"
                            .into(),
                    extra_fields: vec![
                        proto::LoginField {
                            key: "uid".into(),
                            label: "账号 uid（可选）".into(),
                            placeholder: "用于显示".into(),
                            optional: true,
                            multiline: false,
                        },
                        proto::LoginField {
                            key: "sessionToken".into(),
                            label: "网页会话 token（可选）".into(),
                            placeholder: "astron-session.json 的 token；填了才有余额与签到".into(),
                            optional: true,
                            multiline: false,
                        },
                        proto::LoginField {
                            key: "accountId".into(),
                            label: "账号面 accountId（可选）".into(),
                            placeholder: "与 sessionToken 一起填".into(),
                            optional: true,
                            multiline: false,
                        },
                        proto::LoginField {
                            key: "ssoSessionId".into(),
                            label: "ssoSessionId（可选）".into(),
                            placeholder: "Cookie 会话的补充材料".into(),
                            optional: true,
                            multiline: false,
                        },
                    ],
                },
            ])
            .with_models(models::fallback_models())
            .with_account_features(proto::AccountFeatures {
                usage: true,
                credit_detail: false,
                credit_expiry: false,
                usage_detail: false,
                edition: false,
                identifier: vec!["uid".into(), "accountId".into()],
                expiry: Vec::new(),
                long_lived: false,
                claim: false,
                plan_channel: false,
                email_as_name: false,
                concurrency_default: None,
                default_limiters: None,
            })
            .with_checkin(proto::CheckinInfo {
                description: Some(
                    "AStudio 每日登录积分：执行官方客户端同款账号初始化，\
                     分别核对通用积分与星火积分；未观测到新增奖励不标记已签"
                        .into(),
                ),
                modes: vec![proto::CheckinMode::Checkin],
                ..Default::default()
            });
        meta.order = 32;
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
        Ok(accounts::public(secret))
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(accounts::identity(secret))
    }

    /// 动态登录方式（网关「添加账号」弹窗按它渲染）。
    fn admin_action<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
        action: &'a str,
        _payload: &'a Value,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            match action {
                "auth.methods" => Ok(login::login_methods()),
                other => Err(agent2api_provider_sdk::unsupported(format!(
                    "AStudio 不支持管理动作「{other}」"
                ))),
            }
        })
    }

    // ── 登录（手机号 / 账号密码，本机内嵌登录页）──────────────

    fn login_start<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        method: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async move {
            if method != "browser" {
                return Err("AStudio 的网页登录请用「手机号 / 账号密码登录」".into());
            }
            let index = payload.get("methodIndex").and_then(Value::as_u64);
            match index {
                // 1 = 手机号验证码 / 账号密码（本机登录页 + 极验）
                Some(1) => {
                    let proxy = payload
                        .get("proxy")
                        .filter(|value| !value.is_null())
                        .and_then(|value| serde_json::from_value(value.clone()).ok());
                    login::start_web(proxy, ctx.env.proxy.clone()).await
                }
                // 2 = modelBearerToken，走「粘贴凭证」而不是登录流程
                Some(2) => {
                    Err("modelBearerToken 请用「粘贴 modelBearerToken」方式添加".to_string())
                }
                // 0 是已移除的「读取桌面端登录态」；旧弹窗缓存也可能走到这里
                _ => Err("这条登录方式已不再提供，请关闭弹窗后重新打开".to_string()),
            }
        })
    }

    fn login_poll<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move { Ok(login::poll(flow)) })
    }

    fn login_cancel<'a>(&'a self, _ctx: &'a PluginCtx, flow: &'a str) -> SendFuture<'a, ()> {
        Box::pin(async move {
            login::cancel(flow);
            Ok(())
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
                if let Ok(list) = ctx.store_list(PROVIDER_ID).await {
                    if let Some(first) = list.first() {
                        account_id = first.id.clone();
                    }
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

    // ── 余额 / 签到 ───────────────────────────────────────────

    fn usage<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::UsageReport> {
        Box::pin(async move { usage::query(ctx, account).await })
    }

    fn checkin<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(async move { checkin::checkin(ctx, account).await })
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

    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        let message = body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .or_else(|| body.get("message").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        if status == 401 || status == 403 {
            return proto::ErrorClass::TokenExpired { message };
        }
        let lower = message.to_ascii_lowercase();
        if status == 429
            || lower.contains("quota")
            || lower.contains("balance")
            || lower.contains("credit")
            || lower.contains("额度")
            || lower.contains("积分")
        {
            return proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: None,
                status,
            };
        }
        proto::ErrorClass::Fatal {
            status,
            message,
            upstream_code: None,
        }
    }

    /// 思考档位写进 `reasoning_effort`（`responses.rs` 会翻成 `reasoning.effort`）。
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
                reason: "AStudio 不支持该思考档位".into(),
            }
        }
    }
}
