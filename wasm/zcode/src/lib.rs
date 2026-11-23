//! 国内版 ZCode：独立账号目录、双套餐通道与赠送套餐领取。
//! 凭据/验证码限定账号或登录流程，既不读取本机环境，也不共享国际版身份。
//! 未声明 accounts.identity.v1 是有意的：旧版无 userId 的手动凭据返回空身份，
//! 由宿主按凭据去重；不能为了新契约把这些仍可使用的账号拒之门外。

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
mod login;
mod login_key;
mod models;
mod network;
mod prompt;
mod quota;
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
    let meta = json!({ "id": "zcode", "label": "ZCode 国内版", "order": 13,
        "capabilities": { "chat": true, "stateful": false, "refresh": false,
            "model_refresh": true, "usage": true, "checkin": false, "claims": false,
            "web_login": true, "sse_model_rewrite": true, "refresh_uses_account": true,
            "supports_default_model": false, "env_credentials": false },
        "login_methods": [{ "kind": "browser", "button": "网页登录", "hint": "在 ZCode 授权页完成国内版账号登录" }],
        "account_features": { "usage": true, "credit_detail": true, "claim": false,
            "plan_channel": true, "edition": true, "identifier": ["userId"], "expiry": ["expiresAt"] },
        "desktop_launch": desktop_launch::summary(),
        "admin_panels": ["captcha_pool"],
        "default_models": models::list(), "settings_schema": config::schema(),
        "settings_group": { "label": "ZCode 国内版" }
    });
    attach(
        meta,
        vec![
            class("rate_limit", &[429], &["1302", "1305", "3008", "3009", "3010"]),
            class("quota", &[], &["1005", "1113", "1308", "1310"]),
            class("auth", &[401], &["1000", "1001", "1002", "1003", "1004", "3007", "3012"]),
            class("unsupported", &[403, 501], &["1309", "1311", "1313", "1314", "1315"]),
            class("bad_request", &[400, 413, 422], &["3001"]),
            class("content", &[], &["1301", "content_filter"]),
        ],
        vec![
            rule("zcode_captcha_3007", "上游人机验证（3007）", "触发官方人机验证时先冷却当前账号并切换其他可用账号。", "auth", &[428], "3007", "switch_account", "fixed", 900, true, 3600, "account"),
            rule("zcode_risk_3012", "上游风控拒绝（3012）", "触发风控提示时冷却当前账号并切换其他账号，不反复重试同一账号。", "auth", &[], "3012", "switch_account", "fixed", 1800, true, 7200, "account"),
            rule("zcode_plan_unsupported", "套餐或模型权限受限", "当前套餐不支持该模型或订阅已到期时，将该账号优先级调到最低并切换其他账号。", "unsupported", &[], "", "switch_account", "deprioritize", 1800, true, 7200, "account"),
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
        "token.refresh" => Err(
            Error::http(401, "ZCode 登录态不能自动续期，请重新登录该账号").with_code("unsupported"),
        ),
        "login.start" => login::start(&params),
        "login.poll" => login::poll(&params),
        "login.cancel" => login::cancel(),
        "models.list" => models::current(&params),
        "models.refresh" => models::refresh(&params),
        "usage.query" => usage::query(),
        "checkin.claim" => checkin::run(),
        "claim.status" => claim::preview(true),
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
        "claim.preview" => claim::preview(true),
        "claim.captcha-config" => captcha::configuration(true),
        "claim.claim" => claim::manual(payload, true),
        "claim.verification.poll" => verification::poll(payload),
        "claim.verification.complete" => verification::complete(payload),
        "captcha.stats" => captcha::stats(),
        "captcha.push" => captcha::push(payload),
        "captcha.cancel" => captcha::cancel(),
        _ => Err(Error::unsupported()),
    }
}
