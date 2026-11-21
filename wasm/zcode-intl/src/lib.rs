//! 独立 ZCode 国际版：只拥有 zcode-intl 身份，不携带国内账号/设置/目录命名空间。
//! 在线目录来自官方公开客户端配置；无自动续期，通用 Anthropic 转换由受控 SDK 承担。
//! 有意不要求 accounts.identity.v1：旧版没有 userId 的手动凭据仍可由宿主去重。

mod account;
mod activation;
mod captcha;
mod chat;
mod checkin;
mod claim;
mod claim_attempt;
mod claim_sources;
mod claim_state;
mod claim_types;
mod config;
mod desktop_launch;
mod errors;
mod i18n;
mod login;
mod login_key;
mod models;
mod network;
mod prompt;
mod reasoning;
mod request_context;
mod usage;
mod usage_billing;
mod usage_monitor;
mod usage_sources;
mod verification;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
sdk::export_provider!(handle);

fn metadata() -> Value {
    json!({ "id": network::PROVIDER, "label": "ZCode 国际版", "order": 14,
        "capabilities": { "chat": true, "stateful": false, "refresh": false,
            "model_refresh": true, "usage": true, "checkin": true, "claims": true,
            "web_login": true, "sse_model_rewrite": true, "refresh_uses_account": true,
            "supports_default_model": false, "env_credentials": false },
        "login_methods": [{ "kind": "browser", "button": "网页登录", "hint": "在 Z.AI 授权页完成 ZCode 国际版账号登录" }],
        "account_features": { "usage": true, "credit_detail": true, "claim": true,
            "plan_channel": true, "edition": true, "identifier": ["userId"], "expiry": ["expiresAt"] },
        "desktop_launch": desktop_launch::summary(),
        "admin_panels": ["captcha_pool"],
        "checkin": { "description": "每日赠送套餐领取（以上游活动窗口为准）· 国际版",
            "modes": ["checkin"], "daily": true, "plans": true },
        "default_models": models::list(), "settings_schema": config::schema(),
        "settings_group": { "label": "ZCode 国际版" }
    })
}

fn handle(method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()])),
        "ping" => Ok(json!(true)),
        "provider.info" => Ok(metadata()),
        "desktop.launch.check" => desktop_launch::check(),
        "desktop.launch.prepare" => desktop_launch::prepare(),
        "account.normalize" => account::normalize(&params),
        "account.view" => account::view(&account::decode(&params["secret"])?),
        "account.identity" => Ok(json!(account::identity(&account::decode(
            &params["secret"]
        )?))),
        "token.ensure" => account::ensure().map(|token| json!(token)),
        "token.expiring" => Ok(json!(false)),
        "token.refresh" => Err(Error::http(
            401,
            "ZCode 国际版登录态不能自动续期，请重新登录该账号",
        )
        .with_code("unsupported")),
        "login.start" => login::start(&params),
        "login.poll" => login::poll(&params),
        "login.cancel" => login::cancel(),
        "models.list" => models::snapshot(),
        "models.refresh" => models::refresh(params["force"] == true),
        "usage.query" => usage::query(),
        "checkin.claim" => checkin::run(),
        "claim.status" => claim::preview(true, false),
        "claim" => claim::manual(
            &json!({ "planId": params["claim"].as_str().unwrap_or("") }),
            false,
        ),
        "conversation" => {
            chat::forward(params)?;
            Ok(Value::Null)
        }
        "classify" => Ok(errors::classify(
            params["status"].as_u64().unwrap_or(502) as u16,
            &params["body"],
        )),
        "is_error_response" => Ok(json!(errors::is_error_response(&params))),
        "error.policy" => Ok(
            json!({ "stop": errors::risk(params["status"].as_u64().unwrap_or(502) as u16, &params["body"]) }),
        ),
        "retry.decision" => Ok(
            if errors::risk(
                params["status"].as_u64().unwrap_or(502) as u16,
                &params["body"],
            ) {
                json!({ "action": "stop", "reason": "上游风控拒绝，停止重发、续期和账号轮换" })
            } else {
                json!({ "action": "default" })
            },
        ),
        "retry_advice" => Ok(Value::Null),
        "refusal_markers" => Ok(json!({ "finishes": [], "markers": [] })),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "reasoning_patch" => Ok(reasoning::patch(&params)),
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        "admin.action" => admin(&params),
        _ => Err(Error::unsupported()),
    }
}

fn admin(params: &Value) -> Result<Value> {
    let payload = &params["payload"];
    match params["action"].as_str().unwrap_or("") {
        "auth.methods" => Ok(login::methods(
            payload["locale"].as_str().unwrap_or("zh-CN"),
        )),
        "network.user-agent" => Ok(json!({ "accountWide": true })),
        "quota.sources" => Ok(usage::sources(&payload["report"])),
        "prompt.info" => prompt::info(),
        "claim.preview" => claim::preview(true, false),
        "claim.captcha-config" => captcha::configuration(true),
        "claim.claim" => claim::manual(payload, true),
        "claim.verification.poll" => verification::poll(payload),
        "claim.verification.complete" => verification::complete(payload),
        "claim.verification.cancel" => verification::cancel(payload),
        "captcha.stats" => captcha::stats(),
        "captcha.push" => captcha::push(payload),
        "captcha.cancel" => captcha::cancel(),
        _ => Err(Error::unsupported()),
    }
}
