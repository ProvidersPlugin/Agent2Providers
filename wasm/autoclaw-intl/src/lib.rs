//! 单身份 AutoClaw 国际版 Wasm。没有国内凭据、目录、回调或网络备用出口。
//! Google / Zai 使用主程序托管的 Aliyun V2 与各自固定 loopback。
//! 只有本机凭据读取/解密需要固定来源 helper；浏览器登录不新增任何原生部署依赖。
mod account;
mod campaigns;
mod chat;
mod checkin;
mod client;
mod config;
mod desktop;
mod endpoints;
mod errors;
mod headers;
mod login;
mod login_host;
mod login_methods;
mod models;
mod oauth;
mod oauth_urls;
mod profile;
mod promotion;
mod prompt;
mod refresh;
mod subscription;
mod usage;
mod util;
mod wallets;
mod web_login;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
sdk::export_provider!(handle);

fn metadata() -> Value {
    use sdk::error_rules::attach;
    let meta = json!({ "id": endpoints::PROVIDER, "label": "AutoClaw 国际版", "order": 11,
        "capabilities": { "chat": true, "stateful": false, "refresh": true, "model_refresh": true,
            "usage": true, "checkin": true, "claims": true, "web_login": login_host::READY,
            "sse_model_rewrite": true, "sse_strip_newline_chunks": true,
            "anonymous_default_session": false, "system_proxy_when_unset": false,
            "refresh_uses_account": true, "supports_default_model": false, "env_credentials": false },
        "login_methods": [{ "kind": "desktop", "button": "导入本机国际版登录态",
            "hint": "读取国际版官方登录态；续期与安全写回需另行授权，Google / Zai 网页登录使用官方验证与固定回调" }],
        "account_features": { "usage": true, "credit_detail": true, "identifier": ["userId", "email"], "email_as_name": true,
            "expiry": ["expiresAt", "tokenExpiresAt"] },
        "checkin": { "description": "国际版每日签到任务，并核对登录活跃活动奖励", "modes": ["checkin"], "daily": true },
        "settings_schema": config::schema(), "settings_group": { "label": "AutoClaw 国际版" },
        "default_models": models::defaults() });
    attach(meta, vec![], vec![])
}

fn handle(method: &str, params: Value) -> Result<Value> {
    if params
        .get("provider")
        .is_some_and(|value| !value.is_null() && value != "" && value != endpoints::PROVIDER)
    {
        return Err(Error::coded("当前插件只处理 AutoClaw 国际版", "forbidden"));
    }
    match method {
        "init" => Ok(json!([metadata()])),
        "ping" => Ok(json!(true)),
        "provider.info" => Ok(metadata()),
        "account.normalize" => account::normalize(&params),
        "account.view" => account::view(&params["secret"]),
        "account.identity" => Ok(json!(account::identity(&account::decode(
            &params["secret"]
        )?))),
        "token.ensure" => refresh::ensure().map(|token| json!(token)),
        "token.refresh" => refresh::force().map(|token| json!(token)),
        "token.expiring" => refresh::due(sdk::expiring_lead(&params, 0)).map(|due| json!(due)),
        "models.list" => models::cached(),
        "models.refresh" => models::refresh(&params),
        "usage.query" => usage::query(),
        "checkin.claim" => checkin::claim(),
        "claim.status" => campaigns::preview(),
        "claim" => campaigns::claim(params["claim"].as_str().unwrap_or("")),
        "conversation" => {
            chat::forward(params)?;
            Ok(Value::Null)
        }
        "login.start" => login::start(&params),
        "login.poll" => login::poll(&params),
        "login.submit" => login::submit(&params),
        "login.callback" => login::callback(&params),
        "login.cancel" => login::cancel(&params),
        "classify" => Ok(errors::classify(status(&params), &params["body"])),
        "is_error_response" => Ok(json!(
            status(&params) >= 400 || errors::has_error(&params["body"])
        )),
        "error.policy" => Ok(json!({ "stop": false })),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "refusal_markers" => Ok(json!({ "finishes": [], "markers": [] })),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "reasoning_patch" => Ok(
            json!({ "action": "skip", "reason": "AutoClaw 国际版原生透传客户端思考参数，没有独立档位映射" }),
        ),
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        "admin.action" => admin(&params),
        _ => Err(Error::unsupported()),
    }
}
fn status(params: &Value) -> u16 {
    params["status"]
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or(502)
}
fn admin(params: &Value) -> Result<Value> {
    let payload = &params["payload"];
    match params["action"].as_str().unwrap_or("") {
        "auth.methods" => Ok(login_methods::methods(
            payload["locale"].as_str().unwrap_or("zh-CN"),
        )),
        "login.oauth-config" => oauth::captcha_config(),
        // 旧原生管理动作不能绕过宿主 flow/真人验证，且绝不接收 payload 内的验证票据。
        "login.oauth-start" | "login.oauth-callback" => {
            login_host::require_services()?;
            Err(Error::unsupported())
        }
        "network.user-agent" => Ok(json!({ "accountWide": true })),
        "quota.sources" => Ok(usage::sources(&payload["report"])),
        "claim.status" | "activity.status" => campaigns::preview(),
        "benefits.redeem" => campaigns::redeem(payload),
        "promotion.status" => promotion::today(&account::read()?.secret),
        "prompt.info" => Ok(
            json!({ "gatewayNote": "使用国际版官方系统首句并规范化历史载体；原系统正文、思考文本、用户/工具消息与图片保留。上游不接受的输出预算字段不发送。",
            "licenses": [{ "name": "magpie-community/plugins", "text": include_str!("../licenses/magpie-community-MIT.txt") },
                { "name": "OpenClaw", "text": include_str!("../licenses/OpenClaw-MIT.txt") }] }),
        ),
        _ => Err(Error::unsupported()),
    }
}
