//! Qoder 中国版单身份 Wasm。COSY/SSE/设备 PKCE 均通过受控宿主；无 WASI/线程/读盘。
//! 仅国内分支，签到不装 runtime-info，不复用国际/千问账号、目录或设备风控票据。
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
mod endpoints;
mod errors;
mod login;
mod login_methods;
mod messages;
mod models;
mod output;
mod protocol;
mod reasoning;
mod refresh;
mod thinking;
mod usage;
mod util;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
sdk::export_provider!(handle);

fn metadata() -> Value {
    json!({ "id": endpoints::PROVIDER, "label": endpoints::LABEL, "order": 22,
        "capabilities": { "chat": true, "stateful": true, "refresh": true, "model_refresh": true,
            "usage": true, "checkin": true, "claims": true, "web_login": true,
            "sse_model_rewrite": false, "sse_strip_newline_chunks": false, "anonymous_default_session": false,
            "system_proxy_when_unset": false, "refresh_uses_account": true, "supports_default_model": false, "env_credentials": false },
        "login_methods": [{ "kind": "browser", "button": "网页登录", "hint": "Qoder 中国版 PKCE 设备授权，无本机回调" },
            { "kind": "token", "label": "个人访问令牌（PAT）", "placeholder": "Qoder 中国版 PAT", "extra_fields": [] }],
        "account_features": { "usage": true, "credit_detail": true, "edition": true,
            "identifier": ["uid", "userId"], "expiry": ["expiresAt"], "email_as_name": true },
        "checkin": { "description": "国内每日 Credits 活动领取，每天 10:00 更新；限时福利另行点名领取", "daily": true, "modes": ["checkin"] },
        "default_models": catalog::public(&catalog::fallback()),
        "settings_schema": config::schema(), "settings_group": { "label": endpoints::LABEL } })
}
fn handle(method: &str, params: Value) -> Result<Value> {
    if params
        .get("provider")
        .is_some_and(|value| !value.is_null() && value != "" && value != endpoints::PROVIDER)
    {
        return Err(Error::coded("当前插件只处理 Qoder 中国版", "forbidden"));
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
            "Qoder 中国版使用设备授权轮询，不接受授权码或本机回调",
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
        "risk.status" => Ok(
            json!({ "available": false, "source": null, "installSupported": false, "installing": false,
            "message": "Qoder 中国版签到不需要国际版 UMID 组件" }),
        ),
        "risk.install" => Err(Error::http(
            400,
            "本插件仅提供 Qoder 中国版，不安装或运行国际版组件",
        )),
        _ => Err(Error::unsupported()),
    }
}
