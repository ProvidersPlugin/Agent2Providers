//! QuWork 1.6.0：OpenAI Chat 对话、独立账号续期、公开模型目录、积分及奖励。
//! 接口依据为本机安装包的 auth-api-client / model-config 和官方公开配置。
//! 月月送由上游按期自动发放；这里仅展示，兑换码只接受用户主动提交。

mod accounts;
mod benefits;
mod chat;
mod checkin;
mod client;
mod desktop;
mod endpoints;
mod login;
mod models;
mod refresh;
mod usage;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ChatPlan, PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::Value;

use crate::errors::GatewayError;

pub struct QuWorkProvider;

impl Provider for QuWorkProvider {
    fn meta(&self) -> ProviderMeta {
        let mut meta = ProviderMeta::new(endpoints::PROVIDER, "QuWork")
            .with_capabilities(proto::Capabilities {
                chat: true,
                refresh: true,
                model_refresh: true,
                usage: true,
                checkin: true,
                claims: true,
                web_login: true,
                sse_model_rewrite: true,
                refresh_uses_account: false,
                supports_default_model: true,
                ..Default::default()
            })
            .with_login(vec![
                proto::LoginMethod::Desktop {
                    button: "导入 QuWork 本机登录态".into(),
                    hint: Some("请先在运行网关的电脑上登录 QuWork".into()),
                },
                proto::LoginMethod::Token {
                    label: "QuWork 凭证 JSON 或模型凭证".into(),
                    placeholder: "{\"accessToken\":\"…\",\"llmToken\":\"…\"}".into(),
                    extra_fields: Vec::new(),
                },
            ])
            .with_models(models::default_models())
            .with_account_features(proto::AccountFeatures {
                usage: true,
                credit_detail: true,
                credit_expiry: true,
                identifier: vec!["uid".into(), "phone".into()],
                expiry: vec!["expiresAt".into()],
                ..Default::default()
            })
            .with_checkin(proto::CheckinInfo {
                description: Some(
                    "QuWork 每日奖励：先查询领取状态，再领取并回查；结果不明时不重复提交。".into(),
                ),
                modes: vec![proto::CheckinMode::Checkin],
                ..Default::default()
            });
        meta.order = 33;
        meta
    }

    fn normalize_account<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(accounts::normalize(ctx, input))
    }

    fn account_view(&self, secret: &Value) -> Result<Value, String> {
        Ok(accounts::public(secret))
    }
    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(accounts::identity(secret))
    }

    fn build_chat<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        request: proto::ChatRequest,
    ) -> SendFuture<'a, ChatPlan> {
        Box::pin(chat::build(ctx, request))
    }

    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        chat::classify(status, body)
    }

    fn refusal_markers(&self) -> (Vec<String>, Vec<String>) {
        (vec!["content_filter".into()], Vec::new())
    }

    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(refresh::model_token(ctx, account))
    }

    fn refresh_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let secret = refresh::refresh(ctx, account, true)
                .await
                .map_err(GatewayError::into_control_message)?;
            Ok(accounts::text(&secret, &["llmToken"]))
        })
    }

    fn token_expiring<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, bool> {
        Box::pin(async move {
            Ok(refresh::snapshot(ctx, account)
                .await
                .is_ok_and(|secret| refresh::expiring(&secret)))
        })
    }

    fn models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async move { Ok(models::current(ctx).await.list()) })
    }

    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        _account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            Ok(models::refresh(ctx, force)
                .await
                .unwrap_or_else(proto::RefreshOutcome::failed))
        })
    }

    fn reasoning_patch_async<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        level: &'a str,
        model: &'a str,
        body: &'a Value,
    ) -> SendFuture<'a, proto::ReasoningPatch> {
        Box::pin(async move { Ok(chat::reasoning_patch(ctx, level, model, body).await) })
    }

    fn usage<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, Value> {
        Box::pin(usage::query(ctx, account))
    }

    fn checkin<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(checkin::run(ctx, account))
    }

    fn claim_status<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        _refresh: bool,
    ) -> SendFuture<'a, Value> {
        Box::pin(benefits::preview(ctx, account))
    }

    fn claim<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        claim: &'a str,
    ) -> SendFuture<'a, Value> {
        Box::pin(benefits::claim(ctx, account, claim))
    }

    fn admin_action<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        action: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            match action {
                "auth.methods" => Ok(login::methods()),
                "benefits.redeem" => benefits::redeem(ctx, account, payload).await,
                _ => Err(agent2api_provider_sdk::unsupported(
                    "QuWork 不支持该管理动作",
                )),
            }
        })
    }

    fn login_start<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        method: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(login::start(method, payload))
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
}
