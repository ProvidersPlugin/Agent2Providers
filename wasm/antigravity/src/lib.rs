//! Antigravity 独立WASM，2026-11-10迁移本仓原生实现；来源与授权待核验项见包内licenses。
//! 无WASI、无线程、无全局缓存。上游无独立签到/奖励端点的判断沿用原生adapter能力盘点；
//! 仅OAuth、project、目录、额度、内容生成协议，不把正常登录/查询谎报为签到领取。

mod account;
mod aliases;
mod argon;
mod callback;
mod catalog_shape;
mod chat;
mod client;
mod contents;
mod control;
mod credentials;
mod desktop_launch;
mod endpoints;
mod errors;
mod i18n;
mod image_config;
mod image_input;
mod image_output;
mod images;
mod login;
mod model_quotas;
mod model_rules;
mod models;
mod oauth;
mod outbound;
mod project;
mod reasoning;
mod refresh;
mod schema;
mod signatures;
mod sse;
mod stream;
mod usage;
mod usage_windows;
mod util;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
pub const PROVIDER_ID: &str = "antigravity";
sdk::export_provider!(handle);

fn metadata() -> Value {
    json!({ "id": PROVIDER_ID, "label": "Antigravity", "order": 29,
        "capabilities": { "chat": true, "stateful": false, "refresh": true,
            "model_refresh": true, "usage": true, "checkin": false, "claims": false,
            "web_login": true, "sse_model_rewrite": true, "sse_strip_newline_chunks": false,
            "anonymous_default_session": false, "system_proxy_when_unset": true,
            "refresh_uses_account": true, "supports_default_model": false, "env_credentials": false },
        "login_methods": [
            { "kind": "browser", "button": "网页登录", "hint": "在 Google 官方页面完成授权，使用主程序受控本机回调" },
            { "kind": "token", "label": "登录凭据", "placeholder": "粘贴 Google refresh token（以 1// 开头）", "extra_fields": [] }
        ],
        "account_features": { "usage": true, "identifier": ["userId"], "long_lived": true },
        "desktop_launch": desktop_launch::summary(),
        "default_models": catalog_shape::fallback() })
}
fn status(params: &Value) -> u16 {
    params["status"]
        .as_u64()
        .and_then(|s| u16::try_from(s).ok())
        .unwrap_or(502)
}

fn handle(method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()])),
        "provider.info" => Ok(metadata()),
        "ping" => Ok(json!(true)),
        "admin.action" if params["action"] == "auth.methods" => Ok(i18n::methods(
            params["payload"]["locale"].as_str().unwrap_or("zh-CN"),
        )),
        "admin.action" if params["action"] == "quota.sources" => Ok(usage_windows::sources(
            params["payload"]
                .get("report")
                .unwrap_or(&params["payload"]),
        )),
        "desktop.launch.check" => desktop_launch::check(),
        "desktop.launch.prepare" => desktop_launch::prepare(),
        "account.normalize" => account::normalize(&params),
        "account.view" => Ok(account::view(&params["secret"])),
        "account.identity" => Ok(json!(account::identity(&params["secret"]))),
        "token.ensure" | "token.refresh" => {
            let record = refresh::ensure(method == "token.refresh", None)?;
            Ok(json!(credentials::token(&credentials::parse(
                &record["secret"]
            )?)?))
        }
        "token.expiring" => Ok(json!(refresh::expiring()?)),
        "models.list" => models::list(&params),
        "models.refresh" => models::refresh(&params),
        "usage.query" => usage::query(),
        "login.start" => login::start(&params),
        "login.callback" => login::callback(&params),
        "login.poll" => login::poll(&params),
        "login.cancel" => login::cancel(&params),
        "conversation" => {
            chat::forward(&params)?;
            Ok(Value::Null)
        }
        "images.capabilities" => Ok(json!({ "generation": true, "editing": true })),
        "images.generate" => images::generate(&params, false),
        "images.edit" => images::generate(&params, true),
        "classify" => Ok(errors::classify(status(&params), &params["body"])),
        "is_error_response" => Ok(json!(
            status(&params) >= 400 || errors::failed(&params["body"])
        )),
        "error.policy" => {
            let error = errors::upstream(status(&params), &params["body"]);
            Ok(json!({ "stop": errors::local(&error) }))
        }
        "retry.decision" => Ok(errors::retry(&params)),
        "retry_advice" => Ok(errors::advice(&params)),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "refusal_markers" => Ok(json!({ "finishes": [], "markers": [] })),
        "reasoning_patch" => Ok(reasoning::patch(&params)),
        _ => Err(Error::unsupported()),
    }
}
