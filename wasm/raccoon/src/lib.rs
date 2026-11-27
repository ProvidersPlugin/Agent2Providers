//! 旧JWT可续期；IAM无旧刷新授权，不能使用残留refreshToken。
//! 本机只读导入与独立授权的固定续期写回分开，所有账号来自显式宿主上下文。
//! 部分协议处理参考magpie-community MIT实现，保留许可证于licenses/。

mod account;
mod balance;
mod chat;
mod checkin;
mod config;
mod conversation;
mod credentials;
mod desktop;
mod endpoints;
mod errors;
mod fallback;
mod iam;
mod image_size;
mod images;
mod jwt;
mod ledger;
mod login;
mod model_shape;
mod models;
mod network;
mod onboarding;
mod reasoning;
mod refresh_api;
mod refresh_state;
mod session;
mod stream;
mod util;

use agent2api_wasm_sdk::{json, Error, Result, Value};
agent2api_wasm_sdk::export_provider!(handle);

fn metadata() -> Value {
    use agent2api_wasm_sdk::error_rules::{attach, class};
    let meta = json!({ "id": "raccoon", "label": "小浣熊", "order": 7,
        "capabilities": { "chat": true, "stateful": false, "refresh": true, "model_refresh": true, "usage": true,
            "checkin": true, "claims": true, "web_login": true, "sse_model_rewrite": true, "sse_strip_newline_chunks": false,
            "anonymous_default_session": false, "system_proxy_when_unset": false, "refresh_uses_account": true,
            "supports_default_model": false, "env_credentials": false },
        "login_methods": [
            { "kind": "browser", "button": "网页登录", "hint": "桌面应用内完成官方登录后自动接回；网页和手机请使用凭据导入" },
            { "kind": "token", "label": "登录态JSON", "placeholder": "粘贴auth.json或Access Token", "extra_fields": [] },
            { "kind": "desktop", "button": "导入本机登录态", "hint": "读取所连接网关电脑的官方登录文件；本机续期与安全写回需另行授权，IAM 凭据仍须重新登录" }
        ],
        "account_features": { "usage": true, "credit_detail": true, "identifier": ["userId", "uid"], "expiry": ["expiresAt", "tokenExpiresAt"] },
        "checkin": { "description": "桌面端每日积分：setting_info触发，账单核对到账", "daily": true, "modes": ["checkin"], "onboarding": true },
        "usage_query": { "concurrency": 1, "min_interval_ms": 1000 },
        "default_models": fallback::models(), "settings_schema": config::schema(), "settings_group": { "label": "小浣熊" } });
    attach(
        meta,
        vec![
            class("balance", &[402], &["raccoon_balance", "1000007"]),
            class("rate_limit", &[429], &["raccoon_rate_limited"]),
            class("auth", &[401], &["raccoon_auth"]),
            class("content", &[], &["raccoon_content_blocked"]),
        ],
        vec![],
    )
}
fn handle(method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()])),
        "provider.info" => Ok(metadata()),
        "ping" => Ok(json!(true)),
        "admin.action" if params["action"] == "auth.methods" => Ok(login::methods(
            params["payload"]["locale"].as_str().unwrap_or("zh-CN"),
        )),
        "admin.action" if params["action"] == "quota.sources" => {
            Ok(balance::sources(&params["payload"]["report"]))
        }
        "account.normalize" => account::normalize(&params),
        "account.view" => Ok(account::view(&params["secret"])),
        "account.identity" => Ok(json!(account::identity(&params["secret"]))),
        "token.ensure" => Ok(json!(session::ensure(false)?.credential.token)),
        "token.refresh" => Ok(json!(session::ensure(true)?.credential.token)),
        "token.expiring" => Ok(json!(session::expiring(agent2api_wasm_sdk::expiring_lead(&params, 0))?)),
        "login.start" => login::start(&params),
        "login.poll" => login::poll(&params),
        "login.callback" => login::callback(&params),
        "login.cancel" => login::cancel(&params),
        "models.list" => models::list(&params),
        "models.refresh" => models::refresh(&params),
        "usage.query" => balance::query(),
        "checkin.claim" => checkin::claim(),
        "claim.status" => onboarding::status(),
        "claim" => onboarding::claim(),
        "conversation" => {
            chat::forward(&params)?;
            Ok(Value::Null)
        }
        "chat.policy" => Ok(json!({ "respectModelCooldown": true })),
        "images.capabilities" => Ok(json!({ "generation": true, "editing": true })),
        "images.generate" => images::generate(&params, false),
        "images.edit" => images::generate(&params, true),
        "reasoning_patch" => Ok(reasoning::patch(
            params["level"].as_str().unwrap_or(""),
            params["model"].as_str().unwrap_or(""),
            &params["body"],
        )),
        "outbound_reasoning" => Ok(reasoning::outbound(&params["body"])),
        "classify" => Ok(errors::classify(
            params["status"]
                .as_u64()
                .and_then(|value| u16::try_from(value).ok())
                .unwrap_or(502),
            &params["body"],
        )),
        "is_error_response" => {
            let status = params["status"].as_u64().unwrap_or(502);
            let json = params["headers"]
                .as_object()
                .into_iter()
                .flatten()
                .any(|(key, value)| {
                    key.eq_ignore_ascii_case("content-type")
                        && value.as_str().is_some_and(|value| {
                            let mime = value
                                .split(';')
                                .next()
                                .unwrap_or("")
                                .trim()
                                .to_ascii_lowercase();
                            mime == "application/json" || mime.ends_with("+json")
                        })
                });
            Ok(json!(!(200..300).contains(&status) || json))
        }
        "refusal_markers" => Ok(json!({ "finishes": [], "markers": [] })),
        "error.policy" | "retry_advice" => Ok(Value::Null),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "cooldown_models" => Ok(json!({ "models": [] })),
        _ => Err(Error::unsupported()),
    }
}
