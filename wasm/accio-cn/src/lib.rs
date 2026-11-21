//! Accio 国内独立入口。共用 ADK/工具/图片/续期/网络模块通过 crate 绑定国内端点。
//! 两站账号和目录分别隔离，保留原方法索引及合法旧凭据。
//! 未确认可接的每日签到/领取动作，不由旧原生空实现推断上游不存在。
//! 国内额度为 quota 百分比与订阅积分双读数，分桶和总量只按真实上游返回展示。

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod account;
mod catalog;
mod credentials;
mod endpoints;
mod errors;
mod login;

#[path = "../../accio/src/adk.rs"]
mod adk;
#[path = "../../accio/src/chat.rs"]
mod chat;
#[path = "../../accio/src/models.rs"]
mod models;
#[path = "../../accio/src/network.rs"]
mod network;
#[path = "../../accio/src/reasoning.rs"]
mod reasoning;
#[path = "../../accio/src/refresh.rs"]
mod refresh;
#[path = "../../accio/src/request.rs"]
mod request;
#[path = "../../accio/src/errors.rs"]
mod shared_errors;
#[path = "../../accio/src/login.rs"]
mod shared_login;
#[path = "../../accio/src/sse.rs"]
mod sse;
#[path = "../../accio/src/tools.rs"]
mod tools;
#[path = "../../accio/src/usage.rs"]
mod usage;
#[path = "../../accio/src/util.rs"]
mod util;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

sdk::export_provider!(handle);

fn metadata() -> Value {
    json!({ "id": endpoints::ID, "label": endpoints::LABEL, "order": 27,
        "capabilities": { "chat": true, "stateful": true, "refresh": true, "model_refresh": true,
            "usage": true, "web_login": true, "checkin": false, "claims": false,
            "anonymous_default_session": false, "env_credentials": false, "refresh_uses_account": true,
            "supports_default_model": false, "system_proxy_when_unset": false,
            "sse_model_rewrite": false, "sse_strip_newline_chunks": false },
        "login_methods": [
            { "kind": "browser", "button": "网页登录", "hint": "在 Accio 国内站完成授权；远程部署可粘贴完整回调地址。" },
            { "kind": "token", "label": "登录凭据", "placeholder": "国内版 accessToken（JSON 请使用专用导入项）",
                "extra_fields": [{ "key": "refreshToken", "label": "刷新凭据", "optional": true }] }
        ],
        "account_features": { "usage": true, "credit_detail": false, "edition": true,
            "identifier": ["userId"], "expiry": ["expiresAt"], "email_as_name": true },
        "default_models": catalog::fallback()["models"] })
}

fn handle(method: &str, params: Value) -> Result<Value> {
    dispatch(method, params).map_err(errors::domestic)
}

fn dispatch(method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()])),
        "provider.info" => Ok(metadata()),
        "ping" => Ok(json!(true)),
        "admin.action" if params["action"] == "auth.methods" => Ok(login::methods(
            params["payload"]["locale"].as_str().unwrap_or("zh-CN"),
        )),
        "admin.action" if params["action"] == "network.user-agent" => {
            Ok(json!({ "accountWide": true }))
        }
        "admin.action" if params["action"] == "quota.sources" => Ok(usage::sources(
            params["payload"]
                .get("report")
                .unwrap_or(&params["payload"]),
        )),
        "account.normalize" => account::normalize(&params),
        "account.view" => account::view(&params["secret"]),
        "account.identity" => Ok(json!(account::identity(&params["secret"])?)),
        "token.ensure" => Ok(json!(refresh::ready()?.credentials.access)),
        "token.refresh" => Ok(json!(refresh::force()?.credentials.access)),
        "token.expiring" => Ok(json!(credentials::Credentials::parse(
            &sdk::account()?["secret"]
        )?
        .expiring_within(util::now()?, sdk::expiring_lead(&params, 0)))),
        "models.list" => catalog::list(&params),
        "models.refresh" => catalog::refresh(&params),
        "login.start" => login::start(&params),
        "login.callback" => login::callback(&params),
        "login.poll" => login::poll(&params),
        "login.cancel" => shared_login::cancel(&params),
        "usage.query" => usage::query(),
        "conversation" => {
            chat::forward(&params)?;
            Ok(Value::Null)
        }
        "classify" => Ok(errors::classify(
            params["status"].as_u64().unwrap_or(502) as u16,
            &params["body"],
        )),
        "reasoning_patch" => Ok(reasoning::patch(
            params["level"].as_str().unwrap_or(""),
            &params["body"],
        )),
        "is_error_response" => Ok(json!(
            params["status"].as_u64().unwrap_or(502) >= 400
                || network::header(&params["headers"], "content-type").contains("text/html")
                || network::header(&params["headers"], "bxpunish") == "1"
        )),
        "error.policy" => Ok(json!({ "stop": false })),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        _ => Err(Error::unsupported()),
    }
}
