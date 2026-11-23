//! AutoClaw 国内版独立 Wasm：不合并国际账号、缓存或签名出口。
//! 国内官方登录为短信；本机导入保存实时引用，续期与写回需要独立辅助授权。
//! 环境旁路不在 Wasm 读取，桌面实时/enc: 来源保留受控 helper 契约。
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
mod login_methods;
mod models;
mod profile;
mod promotion;
mod prompt;
mod refresh;
mod sms;
mod subscription;
mod usage;
mod util;
mod wallets;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
sdk::export_provider!(handle);

fn metadata() -> Value {
    use sdk::error_rules::attach;
    let meta = json!({ "id": endpoints::PROVIDER, "label": "AutoClaw 国内版", "order": 10,
        "capabilities": { "chat": true, "stateful": false, "refresh": true, "model_refresh": true,
            "usage": true, "checkin": true, "claims": true, "web_login": false,
            "sse_model_rewrite": true, "sse_strip_newline_chunks": true,
            "anonymous_default_session": false, "system_proxy_when_unset": false,
            "refresh_uses_account": true, "supports_default_model": false, "env_credentials": false },
        "login_methods": [{ "kind": "sms", "hint": "国内版使用手机号与 6 位短信验证码；官方风控阻断时请在官方客户端登录后导入" },
            { "kind": "desktop", "button": "导入本机登录态", "hint": "读取所连接网关电脑的官方登录态；续期与安全写回需在插件设置中另行授权，未授权时仍只读" }],
        "account_features": { "usage": true, "credit_detail": true, "identifier": ["userId"],
            "expiry": ["expiresAt", "tokenExpiresAt"] },
        "checkin": { "description": "官方客户端每日签到任务，并核对登录活跃活动奖励", "modes": ["checkin"], "daily": true },
        "settings_schema": config::schema(), "settings_group": { "label": "AutoClaw 国内版" }, "default_models": models::defaults() });
    attach(meta, vec![], vec![])
}

fn handle(method: &str, params: Value) -> Result<Value> {
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
        "login.poll" => login::poll(),
        "login.cancel" => login::cancel(),
        "classify" => Ok(errors::classify(
            params["status"].as_u64().unwrap_or(502) as u16,
            &params["body"],
        )),
        "is_error_response" => Ok(json!(
            params["status"].as_u64().unwrap_or(502) >= 400 || errors::has_error(&params["body"])
        )),
        "error.policy" => Ok(json!({ "stop": false })),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "refusal_markers" => Ok(json!({ "finishes": [], "markers": [] })),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "reasoning_patch" => Ok(
            json!({ "action": "skip", "reason": "AutoClaw 原生透传客户端思考参数，没有独立档位映射" }),
        ),
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        "admin.action" => admin(&params),
        _ => Err(Error::unsupported()),
    }
}
fn admin(params: &Value) -> Result<Value> {
    let payload = &params["payload"];
    match params["action"].as_str().unwrap_or("") {
        "auth.methods" => Ok(login_methods::methods(
            payload["locale"].as_str().unwrap_or("zh-CN"),
        )),
        "login.sms.send" => sms::send(params),
        "login.sms.verify" => sms::verify(params),
        "network.user-agent" => Ok(json!({ "accountWide": true })),
        "quota.sources" => Ok(usage::sources(&payload["report"])),
        "claim.status" | "activity.status" => campaigns::preview(),
        "benefits.redeem" => campaigns::redeem(payload),
        "promotion.status" => {
            let snapshot = account::read()?;
            let result = promotion::today(&snapshot.secret)?;
            if !account::same(&snapshot, &account::read()?) {
                return Err(Error::http(
                    409,
                    "AutoClaw 账号已变化，未返回旧账号活动状态",
                ));
            }
            Ok(result)
        }
        "prompt.info" => Ok(
            json!({ "gatewayNote": "规范化系统提示与上游不能读取的历史字段；保留工具、图片、轮次与思考文本，不因流中断重发对话。",
            "licenses": [{ "name": "magpie-community/plugins", "text": include_str!("../licenses/magpie-community-MIT.txt") },
                { "name": "OpenClaw", "text": include_str!("../licenses/OpenClaw-MIT.txt") }] }),
        ),
        _ => Err(Error::unsupported()),
    }
}
