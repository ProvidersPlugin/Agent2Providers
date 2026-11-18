//! 独立AStudio WASM。Responses转换走主机通用能力，其余厂商协议仅在本目录。
//! 原生无已确认续期/额外领取接口：refresh=false、claims=false；每日登录签到保留真实到账判据。

mod account;
mod auth;
mod captcha;
mod chat;
mod checkin;
mod client;
mod desktop;
mod endpoints;
mod errors;
mod i18n;
mod login;
mod login_state;
mod models;
mod payload;
mod sms;
mod usage;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
pub const PROVIDER_ID: &str = "astudio";
sdk::export_provider!(handle);

fn metadata() -> Value {
    json!({ "id": PROVIDER_ID, "label": "AStudio", "order": 32,
        "capabilities": { "chat": true, "stateful": true, "refresh": false, "model_refresh": true,
            "usage": true, "checkin": true, "claims": false, "web_login": true,
            "sse_model_rewrite": false, "refresh_uses_account": true, "supports_default_model": true,
            "anonymous_default_session": false, "env_credentials": false },
        "login_methods": [{ "kind": "browser", "button": "账号密码登录" }, { "kind": "sms", "hint": "手机号验证码登录" }],
        "account_features": { "usage": true, "identifier": ["uid", "accountId"], "expiry": [], "long_lived": false },
        "refusal_finishes": ["content_filter", "safety"], "refusal_markers": [],
        "checkin": { "daily": true, "description": "每日登录积分：执行官方账号初始化，分别核对通用积分与星火积分；未观测到新增奖励不标记已签", "modes": ["checkin"] },
        "default_models": models::defaults() })
}

fn handle(method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()])),
        "provider.info" => Ok(metadata()),
        "ping" => Ok(json!(true)),
        "admin.action" if params["action"] == "auth.methods" => Ok(i18n::methods(
            params["payload"]["locale"].as_str().unwrap_or("zh-CN"),
        )),
        "admin.action" if params["action"] == "login.sms.send" => sms::send(&params),
        "admin.action" if params["action"] == "login.sms.verify" => sms::verify(&params),
        "admin.action" if params["action"] == "quota.sources" => Ok(usage::sources(
            params["payload"]
                .get("report")
                .unwrap_or(&params["payload"]),
        )),
        "account.normalize" => account::normalize(&params),
        "account.view" => Ok(account::view(&params["secret"])),
        "account.identity" => Ok(json!(account::identity(&params["secret"]))),
        "token.ensure" => Ok(json!(account::model_key(&account::current()?["secret"])?)),
        "token.refresh" => Err(Error::http(
            400,
            "AStudio 没有已确认的自动续期接口，请重新登录或导入",
        )
        .with_code("refresh_unsupported")),
        "token.expiring" => Ok(json!(false)),
        "login.start" => login::start(&params),
        "login.submit" => login::submit(&params),
        "login.poll" => login::poll(&params),
        "login.cancel" => login_state::cancel(),
        "models.list" => models::list(&params),
        "models.refresh" => models::refresh(&params),
        "usage.query" => usage::query(),
        "checkin.claim" => checkin::claim(),
        "conversation" => {
            chat::forward(&params)?;
            Ok(Value::Null)
        }
        "classify" => Ok(errors::classify(
            params["status"].as_u64().unwrap_or(502) as u16,
            &params["body"],
        )),
        "is_error_response" => Ok(json!(params["status"].as_u64().unwrap_or(502) >= 400)),
        "error.policy" => {
            let marker = params["body"]["code"]
                .as_str()
                .or_else(|| params["body"]["error"]["code"].as_str())
                .unwrap_or("");
            Ok(
                json!({ "stop": matches!(marker, "local_precondition" | "native_helper_unavailable"
                | "forbidden" | "plugin_resource_limit" | "aborted" | "timeout") }),
            )
        }
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "refusal_markers" => Ok(json!({ "finishes": ["content_filter"], "markers": [] })),
        "reasoning_patch" => Ok(payload::reasoning(&params)),
        // 原生支持图片输入，不存在已确认的图片生成/编辑端点。
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        _ => Err(Error::unsupported()),
    }
}
