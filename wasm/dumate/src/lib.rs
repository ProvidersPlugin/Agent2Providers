//! 百度搭子使用用户提供的网页 Cookie 换发 app token，直连云端标准 Chat SSE，无 WASI。
//! Copyright (c) 2026 dumate2api contributors. MIT notice: licenses/dumate2api-MIT.txt.
//! 本包及 SDK 依本仓 AGPL-3.0-only；模型、鉴权、积分和媒体协议改写保留 MIT 来源许可。

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod account;
mod activities;
mod chat;
mod checkin;
mod credentials;
mod endpoints;
mod errors;
mod images;
mod login;
mod models;
mod network;
mod request;
mod stream;
mod token;
mod usage;
mod util;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

sdk::export_provider!(handle);

fn metadata() -> Value {
    use sdk::error_rules::{attach, class};
    let meta = json!({ "id": endpoints::PROVIDER, "label": "百度搭子 DuMate", "order": 34,
        "capabilities": { "chat": true, "stateful": false, "refresh": true, "model_refresh": false,
            "usage": true, "checkin": true, "claims": true, "web_login": true,
            "refresh_uses_account": true, "supports_default_model": false,
            "env_credentials": false, "anonymous_default_session": false,
            "system_proxy_when_unset": false, "sse_model_rewrite": true, "sse_strip_newline_chunks": false },
        "login_methods": [{ "kind": "token", "label": "完整 Cookie / 单账号 JSON",
            "placeholder": "BDUSS=…; STOKEN=…", "extra_fields": [] }],
        "account_features": { "usage": true, "credit_detail": true, "credit_expiry": true,
            "claim": true, "identifier": ["uid"], "expiry": ["expiresAt"], "long_lived": false },
        "checkin": { "description": "每日登录积分先查询官方状态，再提交一次并回查；已发放或结果不明时不重复领取，不自动抽奖或代做任务。",
            "daily": true, "modes": ["checkin"] },
        "refusal_finishes": ["content_filter"], "refusal_markers": [],
        "default_models": models::rows() });
    attach(
        meta,
        vec![
            class("balance", &[402], &["dumate_quota_exhausted"]),
            class("rate_limit", &[429], &["dumate_rate_limited"]),
            class("auth", &[401], &["dumate_session_expired"]),
        ],
        vec![],
    )
}

fn admin(params: &Value) -> Result<Value> {
    match params["action"].as_str() {
        Some("auth.methods") => Ok(login::methods(
            params["payload"]["locale"].as_str().unwrap_or("zh-CN"),
        )),
        Some("quota.sources") => Ok(usage::sources(
            params["payload"]
                .get("report")
                .unwrap_or(&params["payload"]),
        )),
        Some("network.user-agent") => Ok(json!({ "accountWide": true })),
        Some("checkin.info") => checkin::info(),
        Some("activity.status" | "claim.status") => activities::preview(),
        Some("activity.tasks") => activities::tasks(),
        Some("activity.lottery") => activities::lottery(),
        Some("usage.records") => usage::records(&params["payload"], false),
        Some("usage.charges") => usage::records(&params["payload"], true),
        Some("activity.draw" | "activity.complete" | "prize.claim") => Err(Error::http(
            400,
            "抽奖、成长任务和实物领奖请在百度搭子官方页面自行操作，插件不代做这些动作",
        )),
        _ => Err(Error::unsupported()),
    }
}

fn handle(method: &str, params: Value) -> Result<Value> {
    if params
        .get("provider")
        .is_some_and(|provider| provider != endpoints::PROVIDER)
    {
        return Err(errors::identity());
    }
    match method {
        "init" => Ok(json!([metadata()])),
        "provider.info" => Ok(metadata()),
        "ping" => Ok(json!(true)),
        "shutdown" => Ok(Value::Null),
        "account.normalize" => account::normalize(&params),
        "account.view" => account::view(&params["secret"]),
        "account.identity" => Ok(json!(account::identity(&params["secret"])?)),
        "token.ensure" => Ok(json!(token::ensure(false)?)),
        "token.refresh" => Ok(json!(token::ensure(true)?)),
        "token.expiring" => {
            let secret = params
                .get("secret")
                .cloned()
                .map(Ok)
                .unwrap_or_else(|| account::current().map(|account| account.secret))?;
            Ok(json!(token::expiring_within(&secret, sdk::expiring_lead(&params, 0))))
        }
        "models.list" => models::list(),
        "models.refresh" => models::refresh(),
        "usage.query" => usage::query(),
        "checkin.claim" => checkin::claim_for(None),
        "claim.status" => activities::preview(),
        "claim" => activities::claim(params["claim"].as_str().unwrap_or("")),
        "conversation" => {
            chat::forward(params)?;
            Ok(Value::Null)
        }
        "images.capabilities" => Ok(json!({ "generation": true, "editing": false })),
        "images.generate" => images::generate(&params),
        "images.edit" => Err(Error::http(
            400,
            "百度搭子参考图编辑尚未接入此 WASM 插件，请使用文生图",
        )),
        "admin.action" => admin(&params),
        "classify" => Ok(errors::classify(
            params["status"]
                .as_u64()
                .and_then(|status| u16::try_from(status).ok())
                .unwrap_or(502),
            &params["body"],
        )),
        "is_error_response" => Ok(json!(
            params["status"].as_u64().unwrap_or(200) >= 400 || errors::failed(&params["body"])
        )),
        "error.policy" => Ok(json!({ "stop": errors::stopped(&params["body"]) })),
        "retry.decision" => Ok(if errors::stopped(&params["body"]) {
            json!({ "action": "stop", "reason": "百度搭子本次响应或原账号状态已改变，不重复提交" })
        } else {
            json!({ "action": "default" })
        }),
        "retry_advice" => Ok(Value::Null),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "refusal_markers" => Ok(json!({ "finishes": ["content_filter"], "markers": [] })),
        "reasoning_patch" => Ok(
            json!({ "action": "skip", "reason": "保留调用方的思考参数，不猜测百度搭子模型的思考档位" }),
        ),
        "login.start" => login::start(&params),
        "login.callback" => login::callback(&params),
        "login.poll" => login::poll(&params),
        "login.cancel" => login::cancel(&params),
        _ => Err(Error::unsupported()),
    }
}
