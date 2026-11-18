//! Codar 独立无 WASI 插件；身份限制原样报告，不修改系统提示词或绕过上游验证。
mod account;
mod activities;
mod chat;
mod checkin;
mod credentials;
mod desktop;
mod endpoints;
mod errors;
mod images;
mod login;
mod login_methods;
mod login_phone;
mod login_state;
mod models;
mod network;
mod reasoning;
mod request;
mod stream;
mod token;
mod usage;
mod util;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
sdk::export_provider!(handle);

fn metadata() -> Value {
    json!({ "id": endpoints::PROVIDER, "label": "科搭 Codar", "order": 35,
        "capabilities": { "chat": true, "stateful": false, "refresh": true, "model_refresh": true,
            "usage": true, "checkin": true, "claims": true, "web_login": true,
            "refresh_uses_account": true, "supports_default_model": true,
            "anonymous_default_session": false, "env_credentials": false,
            "system_proxy_when_unset": false, "sse_model_rewrite": false, "sse_strip_newline_chunks": false },
        "login_methods": [{ "kind": "browser", "button": "微信扫码 / 手机验证码登录" },
            { "kind": "token", "label": "科搭访问令牌 / 单账号 JSON", "placeholder": "eyJ…", "extra_fields": [] },
            { "kind": "desktop", "button": "关联本机科搭登录态", "hint": "只读官方客户端当前登录文件，无需安装或额外权限配置" }],
        "account_features": { "usage": true, "credit_detail": false, "credit_expiry": false,
            "identifier": ["username", "uid"], "expiry": ["expiresAt"], "long_lived": false,
            "claim": true, "concurrency_default": 3, "default_limiters": [] },
        "checkin": { "daily": true, "modes": ["checkin"], "description": "滚动24小时签到，先查询状态再领取；已领取或结果未确认时不重复提交。" },
        "default_models": models::defaults() })
}

fn admin(params: &Value) -> Result<Value> {
    match params["action"].as_str() {
        Some("auth.methods") => Ok(login_methods::methods(
            params["payload"]["locale"].as_str().unwrap_or("zh-CN"),
        )),
        Some("network.user-agent") => Ok(json!({ "accountWide": true })),
        Some("quota.sources") => Ok(usage::sources(
            params["payload"]
                .get("report")
                .unwrap_or(&params["payload"]),
        )),
        Some("checkin.info") => checkin::info(),
        Some("activity.status" | "claim.status") => activities::preview(),
        Some("usage.records" | "usage.ledger") => usage::ledger(&params["payload"]),
        Some("usage.daily") => usage::daily(&params["payload"]),
        _ => Err(Error::unsupported()),
    }
}

fn tasks() -> Value {
    json!([
        { "id": "daily-sign", "label": "科搭每日签到", "enabled": false, "times": ["00:10"], "interval_ms": null, "utc_offset_minutes": 480, "checkin": true },
        { "id": "free-model", "label": "科搭免费模型活动", "enabled": false, "times": [], "interval_ms": 3_600_000, "utc_offset_minutes": 480, "checkin": false }
    ])
}

fn handle(method: &str, params: Value) -> Result<Value> {
    if params
        .get("provider")
        .is_some_and(|provider| provider != endpoints::PROVIDER)
    {
        return Err(errors::identity());
    }
    match method {
        "init" => Ok(json!([metadata()])),
        "provider.info" => Ok(metadata()),
        "ping" => Ok(json!(true)),
        "shutdown" => Ok(Value::Null),
        "account.normalize" => account::normalize(&params),
        "account.view" => account::view(&params["secret"]),
        "account.identity" => Ok(json!(account::identity(&params["secret"])?)),
        "token.ensure" => Ok(json!(token::ensure(false)?)),
        "token.refresh" => Ok(json!(token::ensure(true)?)),
        "token.expiring" => {
            let secret = params
                .get("secret")
                .cloned()
                .map(Ok)
                .unwrap_or_else(|| account::current().map(|current| current.secret))?;
            Ok(json!(token::expiring(&secret)))
        }
        "models.list" => models::list(&params),
        "models.refresh" => models::refresh(),
        "usage.query" => usage::query(),
        "checkin.claim" => checkin::claim(),
        "claim.status" => activities::preview(),
        "claim" => activities::claim(params["claim"].as_str().unwrap_or("")),
        "conversation" => {
            chat::forward(&params)?;
            Ok(Value::Null)
        }
        "login.start" => login::start(&params),
        "login.poll" => login::poll(&params),
        "login.submit" => login::submit(&params),
        "login.cancel" => {
            login_state::flow(&params)?;
            login_state::cancel()
        }
        "images.capabilities" => Ok(json!({ "generation": true, "editing": false })),
        "images.generate" => images::generate(&params),
        "images.edit" => Err(Error::http(
            400,
            "科搭此入口不接受上传文件编辑，请使用生图参数中的HTTPS参考图链接",
        )),
        "admin.action" => admin(&params),
        "classify" => Ok(errors::classify(
            params["status"]
                .as_u64()
                .and_then(|status| u16::try_from(status).ok())
                .unwrap_or(502),
            &params["body"],
        )),
        "is_error_response" => Ok(json!(
            params["status"].as_u64().unwrap_or(200) >= 400 || errors::failed(&params["body"])
        )),
        "error.policy" => Ok(
            json!({ "stop": matches!(errors::code(&params["body"]).as_str(), "account_restricted" | "client_not_supported") }),
        ),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "refusal_markers" => Ok(json!({ "finishes": ["content_filter"], "markers": [] })),
        "reasoning_patch" => Ok(request::reasoning(&params)),
        "tasks.list" => Ok(tasks()),
        "tasks.categories" => Ok(json!({ "daily-sign": "checkin", "free-model": "rewards" })),
        "tasks.run" if params["task"] == "free-model" => {
            let result = activities::claim("codar-free-model")?;
            Ok(sdk::tasks::result(
                matches!(
                    result["outcome"].as_str(),
                    Some("claimed" | "already_claimed" | "inactive")
                ),
                result["message"].as_str().unwrap_or("科搭活动处理完成"),
            ))
        }
        _ => Err(Error::unsupported()),
    }
}
