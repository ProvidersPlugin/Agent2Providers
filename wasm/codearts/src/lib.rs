//! CodeArts 的对话、目录、账号及福利适配，无 WASI。
//! 登录、一次性续期与会话心跳由固定宿主入口执行。

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod account;
mod balance;
mod binding;
mod catalog;
mod catalog_fetch;
mod catalog_state;
mod chat;
mod credentials;
mod dpop;
mod encoding;
mod endpoints;
mod errors;
mod gates;
mod login_methods;
mod models;
mod network;
mod pending;
mod refresh;
mod signer;
mod tool_names;
mod util;
mod welfare;
mod welfare_read;

use agent2api_wasm_sdk::{json, Error, Result, Value};

agent2api_wasm_sdk::export_provider!(handle);

fn metadata() -> Value {
    use agent2api_wasm_sdk::error_rules::{attach, class, rule};
    let meta = json!({ "id": endpoints::PROVIDER, "label": "CodeArts", "order": 28,
        "capabilities": {
            "chat": true, "stateful": true, "refresh": true, "model_refresh": true, "usage": true,
            "checkin": true, "claims": true, "web_login": true,
            "sse_model_rewrite": false, "sse_strip_newline_chunks": false,
            "anonymous_default_session": false, "system_proxy_when_unset": false,
            "refresh_uses_account": true, "supports_default_model": false, "env_credentials": false
        },
        "login_methods": [
            { "kind": "token", "label": "CodeArts 凭据 JSON", "placeholder": "粘贴完整的单账号凭据 JSON",
                "extra_fields": [] }
        ],
        "account_features": { "usage": true, "usage_detail": true, "identifier": ["userId", "uid"],
            "expiry": ["expiresAt"], "long_lived": false, "concurrency_default": 3 },
        "checkin": { "description": "每日福利领取后回读官方活动列表确认；一次性新手礼单独领取，不涉及学生认证、邀请或付费。", "modes": [], "daily": true, "onboarding": true },
        "default_models": [], "migration_status": gates::status() });
    attach(
        meta,
        vec![
            class("rate_limit", &[409, 429], &["quota_limited", "codearts_concurrency_limited"]),
            class("auth", &[401], &["codearts_credential_rejected"]),
            class("transient", &[503], &["codearts_busy"]),
            class("unsupported", &[403, 501], &["codearts_model_rejected", "codearts_not_multimodal", "codearts_channel_unsupported"]),
            class("bad_request", &[400], &["codearts_parameter_rejected", "codearts_model_mismatch", "codearts_tool_name_rejected"]),
            class("content", &[], &["codearts_content_blocked"]),
        ],
        vec![
            rule("codearts_concurrency", "账号并发上限（409）", "账号当前并发数达到上限时短冷却并立即切换其他可用账号。", "rate_limit", &[409], "codearts_concurrency_limited", "switch_account", "fixed", 20, true, 120, "account"),
        ],
    )
}

fn handle(method: &str, params: Value) -> Result<Value> {
    if method != "init" && params.get("provider").is_some() {
        credentials::own_provider(&params)?;
    }
    match method {
        "init" => Ok(json!([metadata()])),
        "provider.info" => Ok(metadata()),
        "ping" | "shutdown" => Ok(Value::Null),
        "account.normalize" => account::normalize(&params),
        "account.view" => account::view(&params["secret"]),
        "account.identity" => Ok(json!(account::identity(&params["secret"])?)),
        "token.ensure" => Ok(json!(refresh::ensure(false)?)),
        "token.refresh" => Ok(json!(refresh::ensure(true)?)),
        "token.expiring" => Ok(json!(refresh::expiring(agent2api_wasm_sdk::expiring_lead(&params, 0))?)),
        "models.list" => catalog::list(&params),
        "models.refresh" => catalog::refresh(&params),
        "usage.query" => balance::query(),
        "admin.action" => admin(&params),
        "conversation" | "chat" => chat::forward(params),
        "login.start" | "login.poll" | "login.callback" | "login.cancel" => {
            agent2api_wasm_sdk::host(&method.replacen("login.", "codearts.login.", 1), params)
        }
        "checkin.claim" => welfare::claim(false, params["manual"] == true),
        "claim" => welfare::claim(true, true),
        "claim.status" => welfare::status_with_refresh(true, params["refresh"] == true),
        "tasks.list" => Ok(json!([])),
        "tasks.run" => Err(gates::claim()),
        "images.capabilities" => {
            Ok(json!({ "generation": false, "editing": false, "reason": gates::SUMMARY }))
        }
        "images.generate" | "images.edit" => Err(gates::chat()),
        "classify" => {
            let secrets = crate::binding::Account::current()
                .map(|account| account.credential.secrets())
                .unwrap_or_default();
            Ok(errors::classify(
                params["status"]
                    .as_u64()
                    .and_then(|n| u16::try_from(n).ok())
                    .unwrap_or(502),
                &params["body"],
                &secrets,
            ))
        }
        "is_error_response" => Ok(json!(
            params["status"].as_u64().unwrap_or(502) >= 400 || errors::failure(&params["body"])
        )),
        "error.policy" => Ok(json!({ "stop": false })),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" | "reasoning_patch" => Ok(Value::Null),
        "refusal_markers" => Ok(json!({ "finishes": [], "markers": [] })),
        "cooldown_models" => Ok(json!({ "models": [] })),
        _ => Err(Error::unsupported()),
    }
}

fn admin(params: &Value) -> Result<Value> {
    match params["action"].as_str() {
        Some("auth.methods") => Ok(login_methods::methods(
            params["payload"]["locale"].as_str().unwrap_or("zh-CN"),
        )),
        Some("migration.status" | "import.requirements") => Ok(gates::status()),
        Some("refresh.status") => refresh::status(),
        Some("welfare.preview") => welfare_read::preview(false),
        Some("onboarding.preview") => welfare_read::preview(true),
        Some("welfare.claim") => welfare::claim(false, true),
        Some("onboarding.claim" | "claim.claim") => welfare::claim(true, true),
        Some("onboarding.status") => welfare::status(true),
        Some("quota.sources") => Ok(balance::sources(&params["payload"]["report"])),
        Some("network.user-agent") => Ok(json!({ "accountWide": true, "signed": false })),
        _ => Err(Error::unsupported()),
    }
}
