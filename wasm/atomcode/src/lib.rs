mod account;
mod chat;
mod errors;
mod i18n;
mod login;
mod models;
mod plans;
mod upstream;
mod usage;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

sdk::export_provider!(handle);

fn metadata() -> Value {
    json!({
        "id": "atomcode", "label": "AtomCode", "order": 14,
        "capabilities": {
            "chat": true, "stateful": false, "refresh": true, "model_refresh": true,
            "usage": true, "checkin": true, "claims": false, "web_login": true,
            "refresh_uses_account": true, "supports_default_model": true
        },
        "login_methods": [{ "kind": "browser", "button": "浏览器登录" }],
        "account_features": { "usage": true, "identifier": ["uid", "userId"], "expiry": ["expiresAt"] },
        "checkin": {
            "description": "领取免费 CodingPlan（按需：套餐有效时只回读额度）",
            "modes": ["checkin"]
        },
        "default_models": []
    })
}

fn handle(method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()])),
        "ping" => Ok(json!(true)),
        "provider.info" => Ok(metadata()),
        "admin.action" if params["action"] == "auth.methods" => Ok(i18n::login_methods(login::methods(),
            params["payload"]["locale"].as_str().unwrap_or("zh-CN"))),
        "admin.action" if params["action"] == "quota.sources" => Ok(json!([])),
        "account.normalize" => account::normalize(&params),
        "account.view" => Ok(account::view(&params["secret"])),
        "account.identity" => Ok(json!(account::identity(&params["secret"]))),
        "token.ensure" => Ok(json!(account::ensure()?)),
        "token.refresh" => Ok(json!(account::refresh()?)),
        "token.expiring" => Ok(json!(account::expiring(&sdk::account()?["secret"]))),
        "login.start" => login::start(&params),
        "login.poll" => login::poll(&params),
        "login.cancel" => Ok(Value::Null),
        "models.list" => models::cached(),
        "models.refresh" => models::refresh(params["force"].as_bool().unwrap_or(false)),
        "usage.query" => usage::query(),
        "checkin.claim" => plans::claim(),
        "conversation" => { chat::forward(&params)?; Ok(Value::Null) }
        "classify" => Ok(errors::classify(params["status"].as_u64().unwrap_or(502) as u16, &params["body"])),
        "is_error_response" => Ok(json!(params["status"].as_u64().unwrap_or(502) >= 400)),
        "error.policy" => Ok(json!({ "stop": params["body"].to_string().contains("ATOMCODE_SIG_MISSING")
            || params["body"].to_string().contains("atomcode_parameter_rejected") })),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "reasoning_patch" => Ok(json!({ "action": "skip", "reason": "按上游原始请求透传思考设置" })),
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        _ => Err(Error::unsupported()),
    }
}
