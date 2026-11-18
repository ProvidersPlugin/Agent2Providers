//! Kiro 独立 WASM，单身份，无 WASI。网页登录由宿主托管双阶段授权；
//! 本机登录通过宿主固定投影读取，续期与安全写回另行授权，不执行 CLI。

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod account;
mod catalog;
mod chat;
mod client;
mod credentials;
mod endpoints;
mod errors;
mod events;
mod eventstream;
mod image;
mod local;
mod login;
mod login_callback;
mod login_methods;
mod login_oauth;
mod login_stage;
mod messages;
mod models;
mod network;
mod reasoning;
mod refresh;
mod request;
mod search;
mod thinking;
mod token_json;
mod token_usage;
mod tool_stream;
mod tools;
mod usage;
mod util;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
sdk::export_provider!(handle);

fn metadata() -> Value {
    json!({ "id": endpoints::ID, "label": "Kiro", "order": 31,
        "capabilities": { "chat": true, "stateful": true, "refresh": true, "model_refresh": true,
            "usage": true, "checkin": false, "claims": false, "web_login": true,
            "sse_model_rewrite": false, "sse_strip_newline_chunks": false, "anonymous_default_session": false,
            "system_proxy_when_unset": false, "refresh_uses_account": true, "supports_default_model": true, "env_credentials": false },
        "login_methods": [
            { "kind": "browser", "button": "Kiro网页登录", "hint": login_methods::WEB },
            { "kind": "desktop", "button": "导入kiro-cli / Kiro IDE登录态", "hint": "读取并关联官方本机登录；续期与安全写回需在插件设置中另行授权，不执行 CLI。" },
            { "kind": "token", "label": "Kiro API key或凭据JSON", "placeholder": "{\"apiKey\":\"ksk_…\"}", "extra_fields": [] }
        ],
        "account_features": { "usage": true, "credit_detail": true, "credit_expiry": false,
            "usage_detail": false, "edition": false, "identifier": ["accountId", "uid"], "expiry": ["expiresAt"],
            "long_lived": false, "claim": false, "plan_channel": false, "email_as_name": true },
        "default_models": models::fallback() })
}
fn handle(method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()])),
        "provider.info" => Ok(metadata()),
        "ping" => Ok(json!(true)),
        "account.normalize" => account::normalize(&params),
        "account.view" => account::view(&params["secret"]),
        "account.identity" => Ok(json!(account::identity(&params["secret"])?)),
        "token.ensure" => Ok(json!(refresh::ready()?.credentials.access)),
        "token.refresh" => Ok(json!(refresh::force()?.credentials.access)),
        "token.expiring" => Ok(json!(refresh::expiring()?)),
        "models.list" => catalog::list(&params),
        "models.refresh" => catalog::refresh(&params),
        "usage.query" => usage::query(),
        "login.start" => login::start(&params),
        "login.callback" => login::callback(&params),
        "login.poll" => login::poll(&params),
        "login.cancel" => login::cancel(&params),
        "conversation" => {
            chat::forward(&params)?;
            Ok(Value::Null)
        }
        "admin.action" => match params["action"].as_str() {
            Some("auth.methods") => Ok(login_methods::methods(params["payload"]["locale"].as_str().unwrap_or("zh-CN"))),
            Some("web-search.capabilities") => Ok(search::capabilities()),
            Some("web-search.search") => search::search(&params["payload"]),
            Some("local.status") => Ok(local::status()),
            Some("network.user-agent") => Ok(json!({ "accountWide": true })),
            _ => Err(Error::unsupported()),
        },
        "classify" => Ok(errors::classify(
            params["status"]
                .as_u64()
                .and_then(|value| u16::try_from(value).ok())
                .unwrap_or(502),
            &params["body"],
        )),
        "reasoning_patch" => Ok(reasoning::patch(
            params["level"].as_str().unwrap_or(""),
            &params["body"],
        )),
        "is_error_response" => Ok(json!(params["status"].as_u64().unwrap_or(502) >= 400)),
        "error.policy" => Ok(json!({ "stop": false })),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        _ => Err(Error::unsupported()),
    }
}
