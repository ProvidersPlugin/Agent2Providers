//! Copyright (c) 2026 edison7009 and EchoBird contributors.
//! Adapted under MIT; the complete notice is in licenses/EchoBird-MIT.txt.
//! 登录态为 Cookie；无凭证续期接口，失效后必须重新登录。

mod accounts;
mod api;
mod catalog;
mod chat;
mod desktop;
mod login;
mod prompt;
mod quota;
mod rewards;
mod stream;
mod upload;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ConversationSink, PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::Value;

pub const PROVIDER_ID: &str = "doubaowork";
pub struct DoubaoWorkProvider;

impl Provider for DoubaoWorkProvider {
    fn meta(&self) -> ProviderMeta {
        let mut meta = ProviderMeta::new(PROVIDER_ID, "豆包工作")
            .with_capabilities(proto::Capabilities {
                stateful: true, model_refresh: true, usage: true, checkin: true,
                claims: true, web_login: true, ..Default::default()
            })
            .with_login(vec![
                proto::LoginMethod::Browser {
                    button: "网页登录".into(), hint: Some("在官方页面登录，成功后自动保存完整 Cookie；网页面板可粘贴 Cookie。".into()),
                },
                proto::LoginMethod::Token {
                    label: "Cookie".into(), placeholder: "sessionid=…; sessionid_ss=…（或 Cookie JSON）".into(), extra_fields: vec![],
                },
                proto::LoginMethod::Desktop {
                    button: "导入本机登录态".into(), hint: Some("读取本机已登录客户端，需使用 --remote-debugging-port=9342 启动；不修改或重启客户端。".into()),
                },
            ])
            .with_account_features(proto::AccountFeatures {
                usage: true, credit_detail: true, credit_expiry: true,
                identifier: vec!["userId".into()], default_limiters: Some(vec![]), ..Default::default()
            })
            .with_checkin(proto::CheckinInfo {
                description: Some("豆包工作每日奖励 · 领取后回查今日签到状态，奖励额度以官方活动为准".into()),
                modes: vec![proto::CheckinMode::Checkin], onboarding: true, ..Default::default()
            });
        meta.order = 30;
        meta
    }

    fn normalize_account<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move {
            accounts::normalize(ctx, input)
                .await
                .map_err(crate::errors::GatewayError::into_control_message)
        })
    }

    fn account_view(&self, secret: &Value) -> Result<Value, String> {
        accounts::public(secret).map_err(crate::errors::GatewayError::into_control_message)
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(accounts::identity(secret))
    }

    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let (_, credentials, _) = api::account(ctx, account, false)
                .await
                .map_err(crate::errors::GatewayError::into_control_message)?;
            accounts::cookie_header(&credentials)
                .map_err(crate::errors::GatewayError::into_control_message)
        })
    }

    fn login_start<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _method: &'a str,
        _payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async {
            login::start().map_err(crate::errors::GatewayError::into_control_message)
        })
    }

    fn login_callback<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        callback: proto::LoginCallback,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move {
            login::callback(ctx, callback)
                .await
                .map_err(crate::errors::GatewayError::into_control_message)
        })
    }

    fn login_cancel<'a>(&'a self, _ctx: &'a PluginCtx, flow: &'a str) -> SendFuture<'a, ()> {
        Box::pin(async move {
            login::cancel(flow);
            Ok(())
        })
    }

    fn models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async move {
            catalog::list(ctx, account)
                .await
                .map_err(crate::errors::GatewayError::into_control_message)
        })
    }

    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            match catalog::refresh(ctx, account, force).await {
                Ok(outcome) => Ok(outcome),
                Err(error) => Ok(proto::RefreshOutcome::failed(error.message)),
            }
        })
    }

    fn conversation<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        request: proto::ConversationRequest,
        sink: ConversationSink<'a>,
    ) -> SendFuture<'a, ()> {
        Box::pin(chat::run(ctx, request, sink))
    }

    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        api::classify(status, body)
    }

    fn reasoning_patch(&self, level: &str, _model: &str, body: &Value) -> proto::ReasoningPatch {
        if !body["reasoning_effort"].is_null() {
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
                reason: "豆包工作不支持该思考档位".into(),
            }
        }
    }

    fn usage<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::UsageReport> {
        Box::pin(async move {
            quota::query(ctx, account)
                .await
                .map_err(crate::errors::GatewayError::into_control_message)
        })
    }

    fn checkin<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(async move {
            rewards::checkin(ctx, account)
                .await
                .map_err(crate::errors::GatewayError::into_control_message)
        })
    }

    fn claim_status<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        _refresh: bool,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            rewards::status(ctx, account)
                .await
                .map_err(crate::errors::GatewayError::into_control_message)
        })
    }

    fn claim<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        _claim: &'a str,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            rewards::claim_extras(ctx, account)
                .await
                .map_err(crate::errors::GatewayError::into_control_message)
        })
    }
}
