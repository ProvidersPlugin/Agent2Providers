//! Command Code 独立 WASM 适配：CLI wire 的无状态生成、静态Key与固定本机快照。
//! /provider/v1/models 为公共目录，/alpha/billing/* 为只读额度/套餐查询。
//! 官方Studio网页登录需要专用POST loopback许可，目前不伪造可用OAuth入口；没有Key续期或可靠签到动作。

mod account;
mod chat;
mod config;
mod errors;
mod fingerprint;
mod models;
mod request;
mod stream;
mod usage;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

sdk::export_provider!(handle);

fn metadata() -> Value {
    json!({
        "id": "commandcode", "label": "Command Code", "order": 1,
        "capabilities": {
            "chat": true, "stateful": false, "refresh": false, "model_refresh": true,
            "usage": true, "checkin": false, "claims": false, "web_login": false,
            "sse_model_rewrite": false, "sse_strip_newline_chunks": false,
            "anonymous_default_session": false, "system_proxy_when_unset": false,
            "refresh_uses_account": false, "supports_default_model": false,
            "env_credentials": false
        },
        "login_methods": [{ "kind": "token", "label": "API Key",
            "placeholder": "粘贴 user_ 开头的 API Key", "extra_fields": [] }],
        "account_features": { "identifier": ["id"], "long_lived": true, "usage": true },
        "default_models": models::fallback_models(),
        "settings_schema": config::schema()
    })
}

fn handle(method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()])),
        "ping" => Ok(json!(true)),
        "provider.info" => Ok(metadata()),
        "admin.action" if params["action"] == "auth.methods" => Ok(account::methods(
            params["payload"]["locale"].as_str().unwrap_or("zh-CN"),
        )),
        "admin.action" if params["action"] == "quota.sources" => Ok(json!([])),
        "account.normalize" => account::normalize(&params),
        "account.view" => Ok(account::view(&params["secret"])),
        "account.identity" => Ok(json!(account::identity(&params["secret"])?)),
        // 静态 Key 没有刷新端点；与旧版默认行为一致，刷新调用只重新读取同一 Key。
        "token.ensure" | "token.refresh" => Ok(json!(account::ensure()?)),
        "token.expiring" => Ok(json!(false)),
        "models.list" => models::cached(),
        "models.refresh" => models::refresh(&params),
        "usage.query" => usage::query(),
        "conversation" => {
            chat::forward(&params)?;
            Ok(Value::Null)
        }
        "classify" => Ok(errors::classify(
            params["status"]
                .as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .unwrap_or(502),
            &params["body"],
        )),
        "is_error_response" => Ok(json!(
            params["status"].as_u64().unwrap_or(502) >= 400
                || params["body"]["type"] == "error"
                || params["body"]
                    .get("error")
                    .is_some_and(|value| !value.is_null())
        )),
        "error.policy" => Ok(json!({ "stop": false })),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "reasoning_patch" => {
            Ok(json!({ "action": "skip", "reason": "保留 Command Code 的思考参数与历史思考块" }))
        }
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        _ => Err(Error::unsupported()),
    }
}
