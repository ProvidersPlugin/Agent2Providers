//! 单独的 Cline 免费池 WASM 插件。

#[path = "../../cline-common/account.rs"]
mod account;
#[path = "../../cline-common/chat.rs"]
mod chat;
#[path = "../../cline-common/client.rs"]
mod client;
#[path = "../../cline-common/desktop.rs"]
mod desktop;
#[path = "../../cline-common/endpoints.rs"]
mod endpoints;
#[path = "../../cline-common/errors.rs"]
mod errors;
#[path = "../../cline-common/headers.rs"]
mod headers;
#[path = "../../cline-common/i18n.rs"]
mod i18n;
#[path = "../../cline-common/login.rs"]
mod login;
#[path = "../../cline-common/models.rs"]
mod models;
#[path = "../../cline-common/payload.rs"]
mod payload;
#[path = "../../cline-common/refresh.rs"]
mod refresh;
#[path = "../../cline-common/usage.rs"]
mod usage;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
pub const PROVIDER_ID: &str = "cline-free";
sdk::export_provider!(handle);

fn metadata() -> Value {
    json!({ "id": PROVIDER_ID, "label": "Cline 免费池", "order": 17,
        "capabilities": { "chat": true, "stateful": false, "refresh": true, "model_refresh": true,
            "usage": true, "checkin": false, "claims": false, "web_login": true, "sse_model_rewrite": true,
            "refresh_uses_account": false, "anonymous_default_session": false, "env_credentials": false },
        "login_methods": [{ "kind": "browser", "button": "浏览器登录" }],
        "account_features": { "usage": true, "identifier": ["uid", "account"], "expiry": ["expiresAt"], "default_limiters": [] },
        "admin_panels": ["upstream_headers"], "default_models": models::defaults() })
}

fn handle(method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()])),
        "ping" => Ok(json!(true)),
        "provider.info" => Ok(metadata()),
        "admin.action" if params["action"] == "auth.methods" => Ok(i18n::methods(
            params["payload"]["locale"].as_str().unwrap_or("zh-CN"),
        )),
        "admin.action" if params["action"] == "headers.preview" => headers::preview(),
        "admin.action" if params["action"] == "quota.sources" => Ok(usage::sources(
            params["payload"]
                .get("report")
                .unwrap_or(&params["payload"]),
        )),
        "account.normalize" => account::normalize(&params),
        "account.view" => Ok(account::view(&params["secret"])),
        "account.identity" => Ok(json!(account::identity(&params["secret"]))),
        "token.ensure" => Ok(json!(refresh::token(false)?)),
        "token.refresh" => Ok(json!(refresh::token(true)?)),
        "token.expiring" => Ok(json!(refresh::due()?)),
        "login.start" => login::start(&params),
        "login.poll" => login::poll(&params),
        "login.cancel" => login::cancel(),
        "models.list" => models::cached(),
        "models.refresh" => models::refresh(),
        "usage.query" => usage::query(),
        "conversation" => {
            chat::forward(params)?;
            Ok(Value::Null)
        }
        "classify" => Ok(errors::classify(
            params["status"].as_u64().unwrap_or(502) as u16,
            &params["body"],
        )),
        "is_error_response" => Ok(json!(
            params["status"].as_u64().unwrap_or(502) >= 400
                || errors::event(&params["body"]).is_some()
        )),
        "error.policy" => Ok(json!({ "stop": errors::stop_retry(&params["body"]) })),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "reasoning_patch" => Ok(
            json!({ "action": "skip", "reason": "Cline 保留客户端思考参数，未指定时沿用原生high默认值" }),
        ),
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        _ => Err(Error::unsupported()),
    }
}
