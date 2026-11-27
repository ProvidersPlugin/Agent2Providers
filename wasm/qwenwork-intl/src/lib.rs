//! 千问办公国际单 provider Wasm，仅 qwenwork-intl；不共享国内账号、状态或机器编号。
//! 主机与授权参数来自原生 Site::QwenworkIntl 证据；其余多项是原生同产品推断，非国际实测。
//! 国际钱包、签到、领取均未取证；不借用国内钱包或 Qoder 活动，也不宣称上游不存在。
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
    use sdk::error_rules::{attach, class, rule};
    let meta = json!({ "id": endpoints::PROVIDER, "label": endpoints::LABEL, "order": 25,
        "capabilities": { "chat": true, "stateful": true, "refresh": true, "model_refresh": true,
            "usage": true, "checkin": false, "claims": false, "web_login": true,
            "sse_model_rewrite": false, "sse_strip_newline_chunks": false, "anonymous_default_session": false,
            "system_proxy_when_unset": false, "refresh_uses_account": true, "supports_default_model": false, "env_credentials": false },
        "login_methods": [{ "kind": "browser", "button": "网页登录", "hint": "国际设备 PKCE，无本机回调；完整兼容性尚未国际实测" },
            { "kind": "token", "label": "设备访问凭据", "placeholder": "千问办公国际设备令牌", "extra_fields": [
                { "key": "machineId", "label": "设备编号", "placeholder": "保留国际客户端原设备编号", "optional": true } ] }],
        "account_features": { "usage": true, "credit_detail": true, "edition": true,
            "identifier": ["uid", "userId"], "expiry": ["expiresAt"], "email_as_name": true },
        "default_models": catalog::public(&catalog::fallback()), "refusal_finishes": ["content_filter"],
        "settings_schema": config::schema(), "settings_group": { "label": endpoints::LABEL } });
    attach(
        meta,
        vec![
            class("rate_limit", &[429], &["qwenwork_rate_limited", "qwenwork_model_busy", "10605"]),
            class("balance", &[402], &["qwenwork_balance_insufficient"]),
            class("quota", &[], &["qwenwork_quota_limited"]),
            class("auth", &[401], &["qwenwork_auth_expired", "105"]),
            class("unsupported", &[403, 501], &["qwenwork_forbidden"]),
            class("bad_request", &[400, 413, 422], &["qwenwork_request_rejected"]),
            class("content", &[], &["content_filter"]),
        ],
        vec![
            rule("qwen_busy", "模型排队拥塞（10605）", "模型排队繁忙时优先按上游等待时间短冷却并切换其他账号或提供商。", "rate_limit", &[], "qwenwork_model_busy", "switch_account", "upstream", 30, true, 300, "model"),
            rule("qwen_forbidden_403", "地区或模型无权限（403）", "账号地区或权限不满足时，将该账号当前模型优先级调到最低并换号尝试。", "unsupported", &[403], "qwenwork_forbidden", "switch_account", "deprioritize", 1800, true, 7200, "account"),
        ],
    )
}
fn handle(method: &str, params: Value) -> Result<Value> {
    if params
        .get("provider")
        .is_some_and(|value| !value.is_null() && value != "" && value != endpoints::PROVIDER)
    {
        return Err(Error::coded("当前插件只处理千问办公国际版", "forbidden"));
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
        "token.expiring" => refresh::due(sdk::expiring_lead(&params, 0)).map(|value| json!(value)),
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
            "千问办公国际版使用设备轮询，不接受授权码或本机回调",
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
        "千问办公国际基线没有签到或领取接口证据，本插件未接入；不代表上游没有该能力",
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
        "adaptation.status" => Ok(
            json!({ "internationallyVerified": false, "message": endpoints::EVIDENCE_NOTE,
            "walletsStatus": "not_verified", "walletsNote": endpoints::WALLETS_NOTE, "machine": machine::status() }),
        ),
        "claim.status" | "claim.claim" => Err(unverified_activity()),
        _ => Err(Error::unsupported()),
    }
}
