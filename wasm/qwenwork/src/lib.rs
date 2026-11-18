//! 千问办公国内版独立 Wasm：仅 qwenwork；不依赖原生提供商、不读盘或运行程序。
//! 基线 Site::QwenworkCn 没有签到/领取证据（原生能力位关闭且活动路径只属 Qoder）；
//! 不迁移 Qoder 活动，也不将未取证宣传成上游永远不支持。机器 UUID 默认尚未实测。
mod account;
mod auth;
mod catalog;
mod chat;
mod client;
mod config;
mod context;
mod cosy;
mod credentials;
mod endpoints;
mod errors;
mod login;
mod login_methods;
mod machine;
mod messages;
mod models;
mod output;
mod protocol;
mod reasoning;
mod refresh;
mod thinking;
mod usage;
mod util;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
sdk::export_provider!(handle);

fn metadata() -> Value {
    json!({ "id": endpoints::PROVIDER, "label": endpoints::LABEL, "order": 24,
        "capabilities": { "chat": true, "stateful": true, "refresh": true, "model_refresh": true,
            "usage": true, "checkin": false, "claims": false, "web_login": true,
            "sse_model_rewrite": false, "sse_strip_newline_chunks": false, "anonymous_default_session": false,
            "system_proxy_when_unset": false, "refresh_uses_account": true, "supports_default_model": false, "env_credentials": false },
        "login_methods": [{ "kind": "browser", "button": "网页登录", "hint": "千问办公国内设备 PKCE，无本机回调" },
            { "kind": "token", "label": "设备访问凭据", "placeholder": "千问办公国内设备令牌", "extra_fields": [
                { "key": "refreshToken", "label": "刷新令牌", "optional": true, "secret": true },
                { "key": "userId", "label": "用户编号", "optional": true },
                { "key": "machineId", "label": "原设备编号", "optional": true }
            ] }],
        "account_features": { "usage": true, "credit_detail": true, "edition": true,
            "identifier": ["uid", "userId"], "expiry": ["expiresAt"], "email_as_name": true },
        "default_models": catalog::public(&catalog::fallback()),
        "settings_schema": config::schema(), "settings_group": { "label": endpoints::LABEL } })
}
fn handle(method: &str, params: Value) -> Result<Value> {
    if params
        .get("provider")
        .is_some_and(|value| !value.is_null() && value != "" && value != endpoints::PROVIDER)
    {
        return Err(Error::coded("当前插件只处理千问办公国内版", "forbidden"));
    }
    match method {
        "init" => Ok(json!([metadata()])),
        "ping" => Ok(json!(true)),
        "provider.info" => Ok(metadata()),
        "account.normalize" => account::normalize(&params),
        "account.view" => account::view(&params["secret"]),
        "account.identity" => account::identity(&params["secret"]).map(|id| json!(id)),
        "token.ensure" => refresh::ensure(false).map(|snapshot| json!(snapshot.credentials.access)),
        "token.refresh" => refresh::ensure(true).map(|snapshot| json!(snapshot.credentials.access)),
        "token.expiring" => refresh::due().map(|value| json!(value)),
        "models.list" => models::list(&params),
        "models.refresh" => models::refresh(&params),
        "usage.query" => usage::query(),
        "conversation" => {
            chat::forward(&params)?;
            Ok(Value::Null)
        }
        "login.start" => login::start(&params),
        "login.poll" => login::poll(&params),
        "login.cancel" => login::cancel(&params),
        "login.callback" => Err(Error::http(
            400,
            "千问办公国内版使用设备轮询，不接受授权码或本机回调",
        )),
        "classify" => Ok(errors::classify(status(&params), &params["body"])),
        "is_error_response" => Ok(json!(
            status(&params) >= 400 || errors::has_error(&params["body"])
        )),
        "error.policy" => Ok(json!({ "stop": false })),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "refusal_markers" => Ok(json!({ "finishes": [], "markers": [] })),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "reasoning_patch" => Ok(reasoning::patch(&params)),
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        "checkin.claim" | "claim" | "claim.status" => Err(unverified_activity()),
        "admin.action" => admin(&params),
        _ => Err(Error::unsupported()),
    }
}
fn status(params: &Value) -> u16 {
    params["status"]
        .as_u64()
        .and_then(|status| u16::try_from(status).ok())
        .unwrap_or(502)
}
fn unverified_activity() -> Error {
    Error::http(
        400,
        "千问办公国内基线没有签到或领取接口证据，本插件未接入，也不会调用 Qoder 活动",
    )
    .with_code("capability_not_verified")
}
fn admin(params: &Value) -> Result<Value> {
    let payload = &params["payload"];
    match params["action"].as_str().unwrap_or("") {
        "auth.methods" => Ok(login_methods::methods(
            payload["locale"].as_str().unwrap_or("zh-CN"),
        )),
        "network.user-agent" => Ok(json!({ "accountWide": true })),
        "quota.sources" => Ok(usage::sources(&payload["report"])),
        "machine.status" => Ok(machine::status()),
        "claim.status" | "claim.claim" => Err(unverified_activity()),
        _ => Err(Error::unsupported()),
    }
}
