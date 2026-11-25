//! 豆包工作对话、图片、账号和活动适配，无 WASI。
//! 本机快照需显式授权；签到及领奖结果由宿主回读官方状态确认。
//! Copyright (c) 2026 edison7009 and EchoBird contributors. MIT notice: licenses/EchoBird-MIT.txt.
//! 本包及 SDK 依本仓 AGPL-3.0-only；保留 MIT 来源通知，不引入早期 AGPL EchoBird 代码。

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod account;
mod activities;
mod catalog;
mod chat;
mod credentials;
mod endpoints;
mod errors;
mod gates;
mod image_generation;
mod login_methods;
mod models;
mod network;
mod prompt;
mod quota;
mod quota_windows;
mod restriction;
mod stream;
mod util;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

agent2api_wasm_sdk::export_provider!(handle);

fn metadata() -> Value {
    use sdk::error_rules::{attach, class, rule};
    let meta = json!({ "id": endpoints::PROVIDER, "label": "豆包工作", "order": 30,
        "capabilities": {
            "chat": true, "stateful": true, "refresh": false, "model_refresh": true,
            "usage": true, "checkin": false, "claims": true, "web_login": true,
            "sse_model_rewrite": false, "sse_strip_newline_chunks": false,
            "anonymous_default_session": false, "env_credentials": false,
            "system_proxy_when_unset": false, "refresh_uses_account": true, "supports_default_model": false
        },
        "login_methods": [{ "kind": "token", "label": "完整 Cookie / 快照 JSON",
            "placeholder": "sessionid=…; sessionid_ss=…（导入前在线核对身份）", "extra_fields": [] }],
        "account_features": { "usage": true, "credit_detail": true, "credit_expiry": true,
            "identifier": ["userId"], "expiry": [], "long_lived": false, "claim": true,
            "default_limiters": [] },
        "default_models": [], "readiness": gates::status(), "migration_status": gates::status(),
        "checkin": {"description": "豆包工作每日签到已锁定官方桌面端安全校验，仅支持只读查询今日签到状态，请打开豆包工作桌面端手动签到。其他已完成任务可在活动福利逐项领取。",
            "daily": false, "modes": [], "onboarding": false, "plans": false} });
    attach(
        meta,
        vec![
            class("quota", &[429], &["doubaowork_quota_limited", "doubaowork_request_limited"]),
            class("auth", &[401], &["doubaowork_auth_expired", "doubaowork_verification_required"]),
            class("unsupported", &[403, 501], &["doubaowork_subscription_required", "doubaowork_model_restricted"]),
            class("bad_request", &[400], &["doubaowork_invalid_input", "unsupported_parameter"]),
            class("content", &[], &["doubaowork_content_blocked"]),
        ],
        vec![
            rule("doubao_verify", "账号需人机验证", "上游要求在官方页面完成人机验证时，冷却该账号并切换其他账号。", "auth", &[429], "doubaowork_verification_required", "switch_account", "fixed", 1800, true, 7200, "account"),
            rule("doubao_sub_403", "账号未开通模型权限（403）", "当前账号不支持所选订阅专属模型时，将该账号此模型优先级调到最低并换号尝试。", "unsupported", &[403], "", "switch_account", "deprioritize", 1800, true, 7200, "model"),
        ],
    )
}

fn admin(params: &Value) -> Result<Value> {
    match params["action"].as_str() {
        Some("auth.methods") => Ok(login_methods::methods(
            params["payload"]["locale"].as_str().unwrap_or("zh-CN"),
        )),
        Some(
            "foundation.status"
            | "migration.status"
            | "capabilities.status"
            | "import.requirements"
            | "local.status",
        ) => Ok(gates::status()),
        Some("quota.sources") => Ok(quota_windows::sources(
            params["payload"]
                .get("report")
                .unwrap_or(&params["payload"]),
        )),
        Some("activity.status" | "claim.status") => activities::query(),
        Some("network.user-agent") => Ok(json!({ "accountWide": true })),
        Some(
            "claim.claim"
            | "checkin.claim"
            | "benefits.redeem"
            | "subscription.purchase"
            | "subscription.accept",
        ) => Err(gates::activity()),
        _ => Err(Error::unsupported()),
    }
}

fn handle(method: &str, params: Value) -> Result<Value> {
    if method != "init" {
        credentials::own_provider(&params)?;
    }
    match method {
        "init" => Ok(json!([metadata()])),
        "provider.info" => Ok(metadata()),
        "ping" => Ok(json!(true)),
        "shutdown" => Ok(Value::Null),
        "account.normalize" => account::normalize(&params),
        "account.view" => account::view(&params["secret"]),
        "account.identity" => Ok(json!(account::identity(&params["secret"])?)),
        "token.ensure" => Ok(json!(account::ensure()?)),
        // false 表示没有已证实的自动续期时机，不表示 Cookie 永不过期。
        "token.expiring" => Ok(json!(false)),
        "token.refresh" => Err(gates::refresh()),
        "models.list" => catalog::list(&params),
        "models.refresh" => catalog::refresh(&params),
        "usage.query" => quota::query(),
        "claim.status" => sdk::host("claim.doubao.tasks", json!({})),
        "claim" => sdk::host("claim.doubao.tasks", json!({})),
        "checkin.claim" => activities::checkin_readonly(),
        "chat" | "conversation" => chat::forward(&params),
        "login.start" => login_methods::start(&params),
        "login.callback" => {
            sdk::cookie_login::callback(endpoints::PROVIDER, &params, account::normalize)
        }
        "login.poll" => login_methods::poll(&params),
        "login.cancel" => sdk::cookie_login::cancel(&params),
        "images.capabilities" => Ok(json!({ "generation": true, "editing": false, "ready": true })),
        "images.generate" => image_generation::generate(&params),
        "images.edit" => Err(Error::http(
            400,
            "豆包工作生图暂不支持图片编辑，请使用支持编辑的图像模型",
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
            params["status"].as_u64().unwrap_or(502) >= 400
                || errors::business(&params["body"]).is_some_and(|code| code != 0)
                || errors::verification(&params["body"])
        )),
        "error.policy" => Ok(json!({ "stop": false })),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "refusal_markers" => Ok(json!({ "finishes": [], "markers": [] })),
        "reasoning_patch" => {
            let level = params["level"].as_str().unwrap_or("");
            if !params["body"]["reasoning_effort"].is_null() {
                Ok(json!({"action":"skip","reason":"客户端已指定思考档位"}))
            } else if matches!(level, "low" | "medium" | "high" | "xhigh" | "max") {
                Ok(json!({"action":"set","field":"reasoning_effort","value":level}))
            } else {
                Ok(json!({"action":"skip","reason":"豆包工作不支持该思考档位"}))
            }
        }
        _ => Err(Error::unsupported()),
    }
}
