mod account;
mod chat;
mod checkin;
mod errors;
mod i18n;
mod login;
mod models;
mod reasoning;
mod upstream;
mod usage;
mod version;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

sdk::export_provider!(handle);

fn metadata() -> Value {
    use sdk::error_rules::{attach, class};
    let meta = json!({
        "id": "lobsterai", "label": "LobsterAI", "order": 6,
        "capabilities": {
            "chat": true, "stateful": false, "refresh": true, "model_refresh": true,
            "usage": true, "checkin": true, "claims": false, "web_login": true,
            "refresh_uses_account": true, "supports_default_model": false,
            "anonymous_default_session": false, "env_credentials": false
        },
        "login_methods": [{ "kind": "browser", "button": "浏览器登录" }],
        "account_features": { "usage": true, "credit_detail": true, "credit_expiry": true,
            "identifier": ["userId", "uid"], "expiry": ["expiresAt"] },
        "checkin": { "description": "每日活动签到（先查询是否可领，领取后复核，不重复提交未知结果）", "daily": true, "modes": ["checkin"] },
        "default_models": models::defaults()
    });
    attach(
        meta,
        vec![
            class("balance", &[402], &["lobsterai_credit"]),
            class("rate_limit", &[429], &["42900"]),
            class("auth", &[401], &["40100", "40101"]),
            class("unsupported", &[403], &["40300"]),
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
        "admin.action" if params["action"] == "quota.sources" => {
            let report = params["payload"]
                .get("report")
                .unwrap_or(&params["payload"]);
            Ok(usage::sources(report))
        }
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
        "login.callback" => login::callback(&params),
        "login.poll" => login::poll(&params),
        "login.cancel" => login::cancel(),
        "models.list" => models::cached(),
        "models.refresh" => models::refresh(&params),
        "usage.query" => usage::query(),
        "checkin.claim" => checkin::claim(),
        "conversation" => {
            chat::forward(params)?;
            Ok(Value::Null)
        }
        "classify" => Ok(errors::classify(
            params["status"].as_u64().unwrap_or(502) as u16,
            &params["body"],
        )),
        "is_error_response" => Ok(json!(
            params["status"].as_u64().unwrap_or(502) >= 400
                || errors::event(&params["body"]).is_some()
        )),
        "error.policy" => Ok(json!({ "stop": false })),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "reasoning_patch" => {
            let level = account::text(&params, &["level", "reasoning"]);
            Ok(if level.is_empty() {
                json!({ "action": "skip", "reason": "没有指定思考档位" })
            } else {
                json!({ "action": "set", "field": "reasoning_effort", "value": level })
            })
        }
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        _ => Err(Error::unsupported()),
    }
}
