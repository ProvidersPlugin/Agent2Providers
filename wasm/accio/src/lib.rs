//! Accio 国际独立 WASM：固定 GLOBAL/intl 身份，不混入国内账号、目录或登录域。
//! 未确认可接入的每日签到/领取动作，保持未接入；不由旧原生空实现推断上游不存在。
//! 网络、平台事实、时钟、随机、锁、状态、凭证与协议转换仅由受控宿主提供。

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod account;
mod adk;
mod catalog;
mod chat;
mod credentials;
mod endpoints;
mod errors;
mod login;
mod models;
mod network;
mod reasoning;
mod refresh;
mod request;
mod sse;
mod tools;
mod usage;
mod util;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

sdk::export_provider!(handle);

fn metadata() -> Value {
    use sdk::error_rules::{attach, class};
    let meta = json!({ "id": endpoints::ID, "label": "Accio 国际版", "order": 26,
        "capabilities": { "chat": true, "stateful": true, "refresh": true, "model_refresh": true,
            "usage": true, "web_login": true, "checkin": false, "claims": false,
            "anonymous_default_session": false, "env_credentials": false, "refresh_uses_account": true,
            "supports_default_model": false, "system_proxy_when_unset": false,
            "sse_model_rewrite": false, "sse_strip_newline_chunks": false },
        "login_methods": [
            { "kind": "browser", "button": "网页登录", "hint": "在 Accio 国际站完成授权；远程部署可粘贴完整回调地址。" },
            { "kind": "token", "label": "登录凭据", "placeholder": "国际版 accessToken（JSON 请使用专用导入项）",
                "extra_fields": [{ "key": "refreshToken", "label": "刷新凭据", "optional": true }] }
        ],
        "account_features": { "usage": true, "credit_detail": false, "edition": true,
            "identifier": ["userId"], "expiry": ["expiresAt"], "email_as_name": true },
        "default_models": models::fallback() });
    attach(
        meta,
        vec![
            class("auth", &[401], &["accio_auth"]),
            class("quota", &[429], &["accio_quota"]),
            class("content", &[], &["accio_content_blocked"]),
        ],
        vec![],
    )
}

fn handle(method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()])),
        "provider.info" => Ok(metadata()),
        "ping" => Ok(json!(true)),
        "admin.action" if params["action"] == "auth.methods" => Ok(login::methods_for(
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
        "login.cancel" => login::cancel(&params),
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
