mod account;
mod chat;
mod errors;
mod i18n;
mod login;
mod models;
mod plans;
mod signing;
mod upstream;
mod usage;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

sdk::export_provider!(handle);

fn metadata() -> Value {
    json!({
        "id": "atomcode", "label": "AtomCode", "order": 14,
        "capabilities": {
            "chat": true, "stateful": false, "refresh": true, "model_refresh": true,
            "usage": true, "checkin": true, "claims": true, "web_login": true,
            "refresh_uses_account": true, "supports_default_model": true,
            "anonymous_default_session": false, "env_credentials": false,
            "system_proxy_when_unset": false
        },
        "login_methods": [{ "kind": "browser", "button": "浏览器登录" }],
        "account_features": { "usage": true, "identifier": ["uid", "userId"], "expiry": ["expiresAt"] },
        "checkin": {
            "description": "领取免费 CodingPlan（按需：套餐有效时只回读额度）",
            "daily": false, "modes": ["checkin"]
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
        "token.expiring" => Ok(json!(account::expiring_within(
            &sdk::account()?["secret"],
            sdk::expiring_lead(&params, 0),
        ))),
        "login.start" => login::start(&params),
        "login.poll" => login::poll(&params),
        "login.cancel" => login::cancel(),
        "models.list" => models::cached(),
        "models.refresh" => models::refresh(params["force"].as_bool().unwrap_or(false)),
        "usage.query" => usage::query(),
        "checkin.claim" => plans::claim(),
        "claim.status" => plans::status(),
        "claim" if params["claim"].as_str().is_none_or(|id| id.is_empty() || id == "codingplan") => plans::claim(),
        "tasks.list" => Ok(json!([{ "id": "codingplan", "label": "领取免费 CodingPlan（按需：套餐有效时只回读额度）",
            "enabled": false, "times": ["10:00"], "utc_offset_minutes": 480, "checkin": false }])),
        "tasks.categories" => Ok(json!({ "codingplan": "rewards" })),
        "tasks.run" if params["task"] == "codingplan" => {
            let report = plans::claim()?;
            Ok(json!({ "success": report["outcome"] == "claimed" || report["outcome"] == "done",
                "message": report["message"], "report": report }))
        }
        "conversation" => { chat::forward(params)?; Ok(Value::Null) }
        "classify" => Ok(errors::classify(params["status"].as_u64().unwrap_or(502) as u16, &params["body"])),
        "is_error_response" => Ok(json!(params["status"].as_u64().unwrap_or(502) >= 400
            || errors::business_error(&params["body"]))),
        "error.policy" => {
            let body = params["body"].to_string();
            let stop = ["ATOMCODE_SIG_", "ATOMCODE_UA_", "ATOMCODE_VER_", "atomcode_client_rejected",
                "atomcode_parameter_rejected", "atomcode_request_policy"].iter().any(|marker| body.contains(marker));
            Ok(json!({ "stop": stop }))
        }
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "reasoning_patch" => Ok(if ["low", "medium", "high", "xhigh"].contains(&params["level"].as_str().unwrap_or("")) {
            json!({ "action": "set", "field": "reasoning_effort", "value": params["level"] })
        } else { json!({ "action": "skip", "reason": "未确认该思考档位，保持原请求" }) }),
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        _ => Err(Error::unsupported()),
    }
}
