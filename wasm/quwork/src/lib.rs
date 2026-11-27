//! QuWork 1.6.0 协议的 WASM 迁移，能力仍待真实账号验收。
//! 本机来源使用宿主固定只读入口，引用与手工独立凭据保持分离。

mod account;
mod benefits;
mod chat;
mod checkin;
mod client;
mod desktop;
mod endpoints;
mod errors;
mod i18n;
mod ledger;
mod login;
mod models;
mod reasoning;
mod refresh;
mod usage;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
sdk::export_provider!(handle);

fn metadata() -> Value {
    use sdk::error_rules::{attach, class};
    let meta = json!({ "id": "quwork", "label": "QuWork", "order": 33,
        "capabilities": { "chat": true, "stateful": false, "refresh": true, "model_refresh": true,
            "usage": true, "checkin": true, "claims": true, "web_login": true,
            "sse_model_rewrite": true, "refresh_uses_account": false, "supports_default_model": true,
            "anonymous_default_session": false, "env_credentials": false },
        "login_methods": [{ "kind": "desktop", "button": "导入 QuWork 本机登录态", "hint": "只读所连接网关电脑的 QuWork 登录文件，无需额外辅助安装或权限配置" },
            { "kind": "token", "label": "QuWork 凭证 JSON 或模型凭证", "placeholder": "llmToken", "extra_fields": [] }],
        "account_features": { "usage": true, "credit_detail": true, "credit_expiry": true,
            "identifier": ["uid", "phone"], "expiry": ["expiresAt"] },
        "checkin": { "description": "QuWork 每日奖励：先查后领、回查确认；结果不明时不重复提交。", "daily": true, "modes": ["checkin"] },
        "refusal_finishes": ["content_filter"], "refusal_markers": [], "default_models": models::defaults() });
    attach(
        meta,
        vec![
            class("balance", &[402], &["quwork_balance"]),
            class("rate_limit", &[429], &["quwork_rate"]),
            class("auth", &[401], &["quwork_account_expired"]),
            class("content", &[], &["quwork_content"]),
        ],
        vec![],
    )
}

fn handle(method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()])),
        "ping" => Ok(json!(true)),
        "provider.info" => Ok(metadata()),
        "admin.action" if params["action"] == "auth.methods" => Ok(i18n::methods(
            params["payload"]["locale"].as_str().unwrap_or("zh-CN"),
        )),
        "admin.action" if params["action"] == "benefits.redeem" => {
            benefits::redeem(&params["payload"])
        }
        "admin.action" if params["action"] == "quota.sources" => Ok(usage::sources(
            params["payload"]
                .get("report")
                .unwrap_or(&params["payload"]),
        )),
        "account.normalize" => account::normalize(&params),
        "account.view" => Ok(account::view(&params["secret"])),
        "account.identity" => Ok(json!(account::identity(&params["secret"]))),
        "token.ensure" => Ok(json!(refresh::model_token()?)),
        "token.refresh" => Ok(json!(account::text(
            &refresh::refresh(true)?,
            &["llmToken"]
        ))),
        "token.expiring" => Ok(json!(refresh::expiring_within(
            &refresh::snapshot()?,
            sdk::expiring_lead(&params, 0),
        ))),
        "login.start" => login::start(&params),
        "login.poll" => login::poll(),
        "login.cancel" => login::cancel(),
        "models.list" => models::cached(),
        "models.refresh" => models::refresh(params["force"].as_bool().unwrap_or(false)),
        "conversation" => {
            chat::forward(params)?;
            Ok(Value::Null)
        }
        "usage.query" => usage::query(),
        "checkin.claim" => checkin::run(),
        "claim.status" => benefits::preview(),
        "claim" => benefits::claim(params["claim"].as_str().unwrap_or("")),
        "classify" => Ok(errors::classify(
            params["status"].as_u64().unwrap_or(502) as u16,
            &params["body"],
        )),
        "reasoning_patch" => reasoning::patch(
            params["level"].as_str().unwrap_or(""),
            params["model"].as_str().unwrap_or(""),
            &params["body"],
        ),
        "is_error_response" => Ok(json!(
            params["status"].as_u64().unwrap_or(502) >= 400
                || errors::event(&params["body"]).is_some()
        )),
        "error.policy" => {
            Ok(json!({ "stop": params["body"].to_string().contains("local_precondition") }))
        }
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        _ => Err(Error::unsupported()),
    }
}
