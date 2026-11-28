//! Qoder 国际版单 provider Wasm。所有网络、随机、状态与账号操作均走受控宿主。
//! 非 UMID 链路完整保留；国际每日活动在真实风控增强步骤门禁，不下载/运行官方组件。
mod account;
mod auth;
mod benefits;
mod campaigns;
mod catalog;
mod chat;
mod checkin;
mod claims;
mod client;
mod config;
mod context;
mod cosy;
mod credentials;
mod daily;
mod endpoints;
mod errors;
mod login;
mod login_methods;
mod messages;
mod models;
mod output;
mod protocol;
mod queue;
mod reasoning;
mod refresh;
mod risk;
mod thinking;
mod usage;
mod util;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
sdk::export_provider!(handle);

fn metadata() -> Value {
    use sdk::error_rules::{attach, class, rule};
    let meta = json!({ "id": endpoints::PROVIDER, "label": endpoints::LABEL, "order": 23,
        "capabilities": { "chat": true, "stateful": true, "refresh": true, "model_refresh": true,
            "usage": true, "checkin": true, "claims": true, "web_login": true,
            "sse_model_rewrite": false, "sse_strip_newline_chunks": false, "anonymous_default_session": false,
            "system_proxy_when_unset": false, "refresh_uses_account": true, "supports_default_model": false, "env_credentials": false },
        "login_methods": [{ "kind": "browser", "button": "网页登录", "hint": "Qoder 国际版 PKCE 设备授权，无本机回调或 UMID 前提" },
            { "kind": "token", "label": "个人访问令牌（PAT）", "placeholder": "Qoder 国际版 PAT", "extra_fields": [] }],
        "account_features": { "usage": true, "credit_detail": true, "edition": true,
            "identifier": ["uid", "userId"], "expiry": ["expiresAt"], "email_as_name": true },
        "checkin": { "daily": true, "description": "国际每日活动北京时间 10:00 开窗；需要在插件设置中授权官方设备验证组件。手动福利另行点名，资格及设备每日限领以上游为准。", "modes": ["checkin"] },
        "default_models": catalog::public(&catalog::fallback()),
        "settings_schema": config::schema(), "settings_group": { "label": endpoints::LABEL } });
    attach(
        meta,
        vec![
            class("rate_limit", &[429], &["qoder_rate_limited"]),
            class("rate_limit", &[503, 429], &["qoder_model_busy", "10605"]),
            class("quota", &[], &["qoder_quota_limited"]),
            class("auth", &[401], &["qoder_auth_expired", "105"]),
            class("unsupported", &[403, 501], &["qoder_forbidden"]),
            class("bad_request", &[400, 413, 422], &["qoder_request_rejected"]),
            class("content", &[], &["content_filter"]),
        ],
        vec![
            rule("qoder_busy", "模型排队拥塞（10605）", "模型排队繁忙时优先按上游等待时间短冷却并切换其他账号或提供商。", "rate_limit", &[], "qoder_model_busy", "switch_account", "upstream", 30, true, 300, "model"),
            rule("qoder_forbidden_403", "地区或模型无权限（403）", "账号地区或权限不满足时，将该账号当前模型优先级调到最低并换号尝试。", "unsupported", &[403], "qoder_forbidden", "switch_account", "deprioritize", 1800, true, 7200, "account"),
        ],
    )
}
fn handle(method: &str, params: Value) -> Result<Value> {
    if params
        .get("provider")
        .is_some_and(|value| !value.is_null() && value != "" && value != endpoints::PROVIDER)
    {
        return Err(Error::coded("当前插件只处理 Qoder 国际版", "forbidden"));
    }
    match method {
        "init" => Ok(json!([metadata()])),
        "ping" => Ok(json!(true)),
        "provider.info" => Ok(metadata()),
        "account.normalize" => account::normalize(&params),
        "account.view" => account::view(&params["secret"]),
        "account.identity" => account::identity(&params["secret"]).map(|id| json!(id)),
        "token.ensure" => refresh::ensure(false).map(|snapshot| json!(snapshot.credentials.access)),
        "token.refresh" => refresh::ensure(true).map(|snapshot| json!(snapshot.credentials.access)),
        "token.expiring" => refresh::due(sdk::expiring_lead(&params, 0)).map(|value| json!(value)),
        "models.list" => models::list(),
        "models.refresh" => models::refresh(&params),
        "usage.query" => usage::query(),
        "checkin.claim" => checkin::claim(),
        "claim.status" => benefits::preview(),
        "claim" => benefits::claim(params["claim"].as_str().unwrap_or("")),
        "conversation" => {
            chat::forward(&params)?;
            Ok(Value::Null)
        }
        "login.start" => login::start(&params),
        "login.poll" => login::poll(&params),
        "login.cancel" => login::cancel(&params),
        "login.callback" => Err(Error::http(
            400,
            "Qoder 国际版使用设备授权轮询，不接受授权码或本机回调",
        )),
        "classify" => Ok(errors::classify(status(&params), &params["body"])),
        "is_error_response" => Ok(json!(
            status(&params) >= 400 || errors::has_error(&params["body"])
        )),
        "error.policy" => Ok(json!({ "stop": false })),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "refusal_markers" => Ok(json!({ "finishes": [], "markers": [] })),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "reasoning_patch" => Ok(reasoning::patch(&params)),
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        "admin.action" => admin(&params),
        _ => Err(Error::unsupported()),
    }
}
fn status(params: &Value) -> u16 {
    params["status"]
        .as_u64()
        .and_then(|status| u16::try_from(status).ok())
        .unwrap_or(502)
}
fn admin(params: &Value) -> Result<Value> {
    let payload = &params["payload"];
    match params["action"].as_str().unwrap_or("") {
        "auth.methods" => Ok(login_methods::methods(
            payload["locale"].as_str().unwrap_or("zh-CN"),
        )),
        "network.user-agent" => Ok(json!({ "accountWide": true })),
        "quota.sources" => Ok(usage::sources(&payload["report"])),
        "claim.status" => benefits::preview(),
        "claim.claim" => benefits::claim(payload["campaignId"].as_str().unwrap_or("")),
        "risk.status" => risk::status(),
        "risk.install" => risk::install(),
        _ => Err(Error::unsupported()),
    }
}
