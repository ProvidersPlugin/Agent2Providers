//! MonkeyCode 国内账号、目录、钱包、登录、签到及受控任务适配，无 WASI。
//! 对话任务仅走固定宿主入口，由本次真实对话请求授权。

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod account;
mod catalog;
mod client;
mod conversation;
mod credentials;
mod endpoints;
mod errors;
mod gates;
mod login;
mod models;
mod subscription;
mod transactions;
mod util;
mod wallet;

use agent2api_wasm_sdk::{json, Error, Result, Value};

agent2api_wasm_sdk::export_provider!(handle);

fn metadata() -> Value {
    json!({
        "id": endpoints::ID, "label": "MonkeyCode 国内版", "order": 20,
        "capabilities": {
            "chat": true, "stateful": true, "refresh": false, "model_refresh": true,
            "usage": true, "checkin": true, "claims": false, "web_login": true,
            "sse_model_rewrite": false, "sse_strip_newline_chunks": false,
            "anonymous_default_session": false, "system_proxy_when_unset": false,
            "refresh_uses_account": true, "supports_default_model": false, "env_credentials": false
        },
        "login_methods": [{
            "kind": "token", "label": "Session／完整 Cookie",
            "placeholder": "monkeycode_ai_session 的值或完整 Cookie；添加前会在线校验",
            "extra_fields": [{ "key": "imageId", "label": "执行环境镜像编号（可选）",
                "placeholder": "留空使用官方默认镜像；不扫描历史任务", "optional": true, "multiline": false }]
        }],
        "account_features": {
            "usage": true, "credit_detail": true, "credit_expiry": false,
            "identifier": ["userId"], "expiry": ["expiresAt"], "long_lived": false,
            "edition": false, "claim": false, "plan_channel": false, "default_limiters": []
        },
        "checkin": { "description": "完成官方计算验证后每日签到，提交前保存防重记录，并回读官方今日状态确认。",
            "modes": [], "daily": true, "onboarding": false, "plans": false, "growth": false },
        "default_models": [], "readiness": gates::status()
    })
}

fn admin(params: &Value) -> Result<Value> {
    match params["action"].as_str() {
        Some("auth.methods") => Ok(login::methods(
            params["payload"]["locale"].as_str().unwrap_or("zh-CN"),
        )),
        Some("foundation.status" | "capabilities.status") => Ok(gates::status()),
        Some("wallet.query") => wallet::wallet_query(),
        Some("wallet.transactions") => wallet::transactions_query(&params["payload"]),
        Some("subscription.query") => wallet::subscription_query(),
        Some("catalog.inspect") => catalog::list(params),
        Some("catalog.target") => catalog::target(params),
        Some("quota.sources") => Ok(wallet::sources(
            params["payload"]
                .get("report")
                .unwrap_or(&params["payload"]),
        )),
        Some("network.user-agent") => Ok(json!({ "accountWide": true })),
        Some(
            "wallet.exchange" | "wallet.recharge" | "subscription.purchase" | "invitation.claim",
        ) => Err(gates::spending()),
        Some("login.sms.send" | "login.sms.verify") => Err(gates::web_login()),
        _ => Err(Error::unsupported()),
    }
}

fn handle(method: &str, params: Value) -> Result<Value> {
    if method != "init"
        && params
            .get("provider")
            .is_some_and(|provider| provider != endpoints::ID)
    {
        return Err(Error::http(
            403,
            "此包只服务 MonkeyCode 国内身份，不跨地区读取账号",
        ));
    }
    match method {
        "init" => Ok(json!([metadata()])),
        "provider.info" => Ok(metadata()),
        "ping" => Ok(json!(true)),
        "shutdown" => Ok(Value::Null),
        "account.normalize" => account::normalize(&params),
        "account.view" => Ok(account::view(&params["secret"])),
        "account.identity" => Ok(json!(account::identity(&params["secret"])?)),
        "token.ensure" => Ok(json!(account::ensure()?)),
        "token.expiring" => Ok(json!(account::expiring(&params)?)),
        "token.refresh" => Err(gates::refresh()),
        "models.list" => catalog::list(&params),
        "models.refresh" => catalog::refresh(&params),
        "usage.query" => wallet::query(),
        "conversation" | "chat" => conversation::forward(&params).map(|_| Value::Null),
        "login.start" => login::start(&params),
        "login.callback" => login::callback(&params),
        "login.poll" => login::poll(&params),
        "login.cancel" => agent2api_wasm_sdk::cookie_login::cancel(&params),
        "checkin.claim" => agent2api_wasm_sdk::monkeycode_checkin::claim(endpoints::ID),
        "claim" | "claim.status" => Err(gates::spending()),
        "admin.action" => admin(&params),
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false,
            "ready": false, "state": "unsupported", "message": gates::images().message })),
        "images.generate" | "images.edit" => Err(gates::images()),
        "classify" => Ok(errors::classify(
            params["status"]
                .as_u64()
                .and_then(|status| u16::try_from(status).ok())
                .unwrap_or(502),
            &params["body"],
        )),
        "is_error_response" => Ok(json!(
            params["status"].as_u64().unwrap_or(502) >= 400
                || util::integer(&params["body"], "code").is_some_and(|code| code != 0)
        )),
        "error.policy" => Ok(json!({ "stop": errors::replay_forbidden(&params["body"]) })),
        "retry.decision" => Ok(if errors::replay_forbidden(&params["body"]) {
            json!({ "action": "stop", "reason": "任务提交与观察结果不能重放，未再次创建" })
        } else {
            json!({ "action": "default" })
        }),
        "retry_advice" => Ok(Value::Null),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "refusal_markers" => Ok(json!({ "finishes": [], "markers": [] })),
        "reasoning_patch" => Ok(
            json!({ "action": "skip", "reason": "任务接口没有可调思考档位；仅转发上游实际产生的思考内容" }),
        ),
        _ => Err(Error::unsupported()),
    }
}
