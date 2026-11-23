//! CatPaw 账号、目录、积分、对话与图片输入适配，无 WASI。
//! 官方登录、工具续接与授权后的本机读取由宿主执行。

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod account;
mod balance;
mod blocks;
mod catalog;
mod conversation;
mod credentials;
mod desktop;
mod endpoints;
mod errors;
mod gates;
mod images;
mod login_methods;
mod messages;
mod models;
mod network;
mod prepare;
// 累计响应转换仅供同包受控消费者，不提供普通 HTTP 回退。
mod stream;
mod stream_snapshot;
mod stream_sse;
mod tools;
mod turn_host;
mod turn_output;
mod turn_request;
mod util;
mod web_login;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

sdk::export_provider!(handle);

fn metadata() -> Value {
    use sdk::error_rules::{attach, class};
    let meta = json!({ "id": endpoints::PROVIDER, "label": "CatPaw", "order": 25,
        "capabilities": {
            "chat": true, "stateful": true, "refresh": false, "model_refresh": true,
            "usage": true, "checkin": false, "claims": false,
            "web_login": true,
            "sse_model_rewrite": false, "sse_strip_newline_chunks": false,
            "anonymous_default_session": false, "env_credentials": false,
            "system_proxy_when_unset": false, "refresh_uses_account": true,
            "supports_default_model": false
        },
        "login_methods": [
            { "kind": "browser", "button": "CatPaw 网页登录", "hint": "使用当前主程序的托管登录步骤" },
            { "kind": "token", "label": "登录令牌 / 凭据 JSON", "placeholder": "仅保存本次明确提供的快照",
                "extra_fields": [{ "key": "uid", "label": "uid / 登录名", "placeholder": "纯令牌导入时必须填写", "optional": true, "multiline": false }] }
        ],
        "account_features": { "usage": true, "credit_detail": false, "credit_expiry": false,
            "identifier": ["uid", "loginName"], "expiry": [], "long_lived": false, "claim": false },
        "migration_status": gates::status(),
        "default_models": models::fallback()["models"] });
    attach(
        meta,
        vec![
            class("auth", &[401], &["catpaw_auth"]),
            class("bad_request", &[400], &["catpaw_invalid_request"]),
        ],
        vec![],
    )
}

fn handle(method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()])),
        "provider.info" => Ok(metadata()),
        "ping" | "shutdown" => Ok(Value::Null),
        "account.normalize" => account::normalize(&params),
        "account.view" => account::view(&params["secret"]),
        "account.identity" => Ok(json!(account::identity(&params["secret"])?)),
        "token.ensure" => {
            let credentials = credentials::current()?;
            credentials.require_main()?;
            Ok(json!(credentials.token))
        }
        "token.refresh" => Err(gates::refresh()),
        // Cookie 无可读有效期，false 仅表示不能确认临期，不能当成永不过期。
        "token.expiring" => Ok(json!(false)),
        "models.list" => catalog::list(&params),
        "models.refresh" => catalog::refresh(&params),
        "usage.query" => balance::query(),
        "conversation" | "chat" => conversation::forward(&params),
        "login.start" => login_methods::start(&params),
        "login.callback" => web_login::callback(&params),
        "login.submit" => Err(errors::invalid("CatPaw 网页登录不接受手工令牌或回调地址，请完成当前官方页面登录")),
        "login.poll" if params["flow"].as_str().is_some_and(|flow| flow.starts_with("cookie_login_")) => desktop::poll(&params),
        "login.poll" => web_login::poll(&params),
        "login.cancel" if params["flow"].as_str().is_some_and(|flow| flow.starts_with("cookie_login_")) => sdk::cookie_login::cancel(&params),
        "login.cancel" => web_login::cancel(&params),
        "checkin.claim" | "claim" | "claim.status" => Err(gates::activity()),
        "admin.action" => admin(&params),
        "classify" => Ok(errors::classify(params["status"].as_u64()
            .and_then(|status| u16::try_from(status).ok()).unwrap_or(502), &params["body"])),
        "error.policy" => Ok(errors::policy(&params["body"])),
        "is_error_response" => Ok(json!(errors::is_error_response(params["status"].as_u64().unwrap_or(502), &params["body"]))),
        "refusal_markers" => Ok(json!({ "finishes": [], "markers": [] })),
        "reasoning_patch" => Ok(models::reasoning_patch(&params)),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false, "reason": gates::SUMMARY })),
        _ => Err(Error::unsupported()),
    }
}

fn admin(params: &Value) -> Result<Value> {
    match params["action"].as_str() {
        Some("auth.methods") => Ok(login_methods::methods(params["payload"]["locale"].as_str().unwrap_or("zh-CN"))),
        Some("import.requirements" | "migration.status" | "local.status") => Ok(gates::status()),
        Some("quota.sources") => Ok(balance::sources(params["payload"].get("report").unwrap_or(&params["payload"]))),
        Some("network.user-agent") => Ok(json!({ "accountWide": true })),
        _ => Err(Error::unsupported()),
    }
}
