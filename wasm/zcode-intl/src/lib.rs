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
    use sdk::error_rules::{attach, class, rule};
    let meta = json!({ "id": network::PROVIDER, "label": "ZCode 国际版", "order": 14,
        "capabilities": { "chat": true, "stateful": false, "refresh": false,
            "model_refresh": true, "usage": true, "checkin": false, "claims": true,
            "web_login": true, "sse_model_rewrite": true, "refresh_uses_account": true,
            "supports_default_model": false, "env_credentials": false },
        "login_methods": [{ "kind": "browser", "button": "网页登录", "hint": "在 Z.AI 授权页完成 ZCode 国际版账号登录" }],
        "account_features": { "usage": true, "credit_detail": true, "claim": true,
            "plan_channel": true, "edition": true, "identifier": ["userId"], "expiry": ["expiresAt"] },
        "desktop_launch": desktop_launch::summary(),
        "admin_panels": ["captcha_pool"],
        "checkin": { "description": "偶尔开放赠送套餐活动（每日签到已下架，有活动时可在活动福利查询并领取）· 国际版",
            "modes": [], "daily": false, "plans": true },
        "default_models": models::list(), "settings_schema": config::schema(),
        "settings_group": { "label": "ZCode 国际版" }
    });
    attach(
        meta,
        vec![
            class("rate_limit", &[429], &["1302", "1303", "3008", "3009"]),
            class("quota", &[], &["1005", "1113"]),
            class("auth", &[401], &["3007", "3012"]),
            class("bad_request", &[400, 413, 422], &["3001"]),
            class("content", &[], &["content_filter"]),
        ],
        vec![
            rule("zcode_captcha_3007", "上游人机验证（3007）", "触发官方人机验证时先冷却当前账号并切换其他可用账号。", "auth", &[], "3007", "switch_account", "fixed", 900, true, 3600, "account"),
            rule("zcode_risk_3012", "上游风控拒绝（3012）", "触发风控提示时冷却当前账号并切换其他账号，不反复重试同一账号。", "auth", &[], "3012", "switch_account", "fixed", 1800, true, 7200, "account"),
        ],
    )
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
        "error.policy" => Ok(json!({ "stop": false })),
        "retry.decision" => Ok(json!({ "action": "default" })),
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
