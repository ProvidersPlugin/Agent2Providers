//! Loomy 独立 WASM，短信/密码与显式本机快照导入均走受限宿主合同。
//! 无 refresh token；不保存密码或按 14 天估算真实会话失效。
//! model protocol 来自真实账号目录，Chat / Messages 输出标准 OpenAI SSE。

mod account;
mod auth;
mod balance;
mod chat;
mod checkin;
mod client;
mod errors;
mod headers;
mod history;
mod images;
mod models;
mod onboarding;
mod reasoning;
mod sign;
mod sms;
mod sms_methods;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

sdk::export_provider!(handle);

pub const ID: &str = "loomy";
pub const ACCOUNT_BASE: &str = "https://account.xfinfr.com";
pub const INTEGRATION_BASE: &str = "https://loomyad.xunfei.cn";
pub const MODEL_BASE: &str = "https://loomyad.xunfei.cn/api/v1";
pub const CLIENT_VERSION: &str = "0.9.39";

fn metadata() -> Value {
    json!({
        "id": ID, "label": "Loomy", "order": 5,
        "capabilities": {
            "chat": true, "stateful": false, "refresh": false, "model_refresh": true,
            "usage": true, "checkin": true, "claims": true, "web_login": false,
            "sse_model_rewrite": false, "sse_strip_newline_chunks": false,
            "anonymous_default_session": false, "system_proxy_when_unset": false,
            "refresh_uses_account": true, "supports_default_model": false, "env_credentials": false
        },
        "login_methods": [{ "kind": "sms", "hint": "填写手机号，再输入 6 位短信验证码" }],
        "account_features": { "usage": true, "credit_detail": true, "identifier": ["uid"], "expiry": [] },
        "checkin": { "description": "每天替账号刷新首次登录积分，不在查询时自动领取",
            "daily": true, "modes": ["checkin"], "onboarding": true },
        "default_models": []
    })
}

fn handle(method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()])),
        "provider.info" => Ok(metadata()),
        "ping" => Ok(json!(true)),
        "admin.action" if params["action"] == "auth.methods" => Ok(sms_methods::methods(
            params["payload"]["locale"].as_str().unwrap_or("zh-CN"),
        )),
        "admin.action"
            if matches!(
                params["action"].as_str(),
                Some("login.sms.send" | "login.sms-send")
            ) =>
        {
            sms::send(&params)
        }
        "admin.action"
            if matches!(
                params["action"].as_str(),
                Some("login.sms.verify" | "login.sms-verify")
            ) =>
        {
            sms::verify(&params)
        }
        "account.normalize" => account::normalize(&params),
        "account.view" => account::view(&params["secret"]),
        "account.identity" => Ok(json!(account::identity(&params["secret"]))),
        "token.ensure" => Ok(json!(account::session()?)),
        "token.refresh" => {
            Err(Error::http(400, "Loomy 会话没有续期接口，请重新登录")
                .with_code("refresh_unavailable"))
        }
        "token.expiring" => Ok(json!(false)),
        "models.list" => models::list(&params),
        "models.refresh" => models::refresh(&params),
        "usage.query" => balance::query(),
        "checkin.claim" => checkin::claim(),
        "claim" => onboarding::claim(params["claim"].as_str().unwrap_or("")),
        "claim.status" => onboarding::status(params["refresh"].as_bool().unwrap_or(false)),
        "conversation" => {
            chat::forward(&params)?;
            Ok(Value::Null)
        }
        "images.capabilities" => Ok(json!({ "generation": true, "editing": true })),
        "images.generate" => images::generate(&params, false),
        "images.edit" => images::generate(&params, true),
        "login.start" => sms::start(&params),
        "login.poll" => sms::poll(&params),
        "login.callback" => sms::callback(&params),
        "login.cancel" => sms::cancel(&params),
        "classify" => Ok(errors::classify(errors::status(&params), &params["body"])),
        "is_error_response" => Ok(json!(errors::is_error_response(
            errors::status(&params),
            &params["headers"]
        ))),
        "reasoning_patch" => Ok(reasoning::patch(
            params["level"].as_str().unwrap_or(""),
            &params["body"],
        )),
        "error.policy" => Ok(Value::Null),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "refusal_markers" => Ok(json!({ "finishes": [], "markers": [] })),
        "cooldown_models" => Ok(json!({ "models": [] })),
        _ => Err(Error::unsupported()),
    }
}
