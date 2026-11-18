//! 库库独立WASM，无WASI/全原生依赖。以当前原生kuku/mod.rs为入口基线。
//! 保留每日30天签到及旧任务领奖；claims=false仅表示无独立批量领取入口。
//! 原生BDUSS无刷新接口，refresh=false不退化业务STOKEN换发。工具/图片
//! 当前未接入，固定think_mode不冒充可调档位；实时文本思考与上游计量如实传回。

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod account;
mod balance;
mod catalog;
mod chat;
mod checkin;
mod credentials;
mod endpoints;
mod engine;
mod errors;
mod gates;
mod login;
mod models;
mod network;
mod request;
mod session;
mod stream;
mod util;

use agent2api_wasm_sdk::{json, Error, Result, Value};
agent2api_wasm_sdk::export_provider!(handle);
fn metadata() -> Value {
    json!({ "id": endpoints::ID, "label": "KukuAI 库库", "order": 19,
        "capabilities": { "chat": true, "stateful": true, "refresh": false, "model_refresh": true,
            "usage": true, "checkin": true, "claims": false, "web_login": true,
            "refresh_uses_account": true, "supports_default_model": false, "env_credentials": false,
            "anonymous_default_session": false, "system_proxy_when_unset": false, "sse_model_rewrite": false,
            "sse_strip_newline_chunks": false },
        "login_methods": [
            { "kind": "browser", "button": "网页登录", "hint": "在桌面应用的官方窗口完成登录，自动取回本次会话 Cookie；网页和手机可粘贴凭据。" },
            { "kind": "token", "label": "Cookie", "placeholder": "BDUSS=…; STOKEN=…（也支持Cookie编辑器JSON）", "extra_fields": [] },
            { "kind": "desktop", "button": "导入本机登录态", "hint": "只读库库官方客户端当前登录 Cookie，保存为快照，不读取系统浏览器。" }
        ],
        "account_features": { "usage": true, "identifier": ["uid"], "expiry": [], "long_lived": false },
        "checkin": { "description": "库库30天签到与已完成旧每日任务领奖；未完成对话任务不会伪报或自动消费", "modes": ["checkin"], "daily": true },
        "default_models": models::fallback()["models"] })
}
fn handle(method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()])),
        "provider.info" => Ok(metadata()),
        "ping" => Ok(json!(true)),
        "account.normalize" => account::normalize(&params),
        "account.view" => account::view(&params["secret"]),
        "account.identity" => Ok(json!(account::identity(&params["secret"])?)),
        "token.ensure" => {
            let (_, c) = session::read()?;
            Ok(json!(c.get("BDUSS")))
        }
        "token.expiring" => Ok(json!(false)),
        "token.refresh" => Err(Error::http(
            400,
            "库库BDUSS没有自动刷新接口，请重新登录或导入Cookie",
        )),
        "models.list" => catalog::list(&params),
        "models.refresh" => catalog::refresh(&params),
        "usage.query" => balance::query(),
        "checkin.claim" => checkin::claim(),
        "login.start" => login::start(&params),
        "login.callback" => login::callback(&params),
        "login.poll" => login::poll(&params),
        "login.cancel" => login::cancel(&params),
        "conversation" => {
            chat::forward(&params)?;
            Ok(Value::Null)
        }
        "admin.action" => match params["action"].as_str() {
            Some("auth.methods") => Ok(login::methods(
                params["payload"]["locale"].as_str().unwrap_or("zh-CN"),
            )),
            Some("import.requirements") => Ok(gates::requirements()),
            Some("quota.sources") => Ok(balance::sources(
                params["payload"]
                    .get("report")
                    .unwrap_or(&params["payload"]),
            )),
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
        "reasoning_patch" => Ok(
            json!({ "action": "skip", "reason": "库库think_mode档位未有可靠证据，保留原生固定值" }),
        ),
        "is_error_response" => Ok(json!(errors::is_error_response(
            params["status"].as_u64().unwrap_or(502) as u16,
            &params["body"]
        ))),
        "error.policy" => Ok(
            json!({ "stop": errors::stop(&params["body"]), "reason": "库库任务执行结果未确认，不自动重发" }),
        ),
        "retry.decision" => Ok(if errors::stop(&params["body"]) {
            json!({ "action":"stop", "reason":"库库任务执行结果未确认，不自动重发" })
        } else {
            json!({ "action": "default" })
        }),
        "retry_advice" => Ok(Value::Null),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        _ => Err(Error::unsupported()),
    }
}
