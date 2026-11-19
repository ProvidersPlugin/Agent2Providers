//! WorkBuddy国际版单身份WASM，副作用均交受控SDK，不依赖WASI或实例常驻。
//! 协议事实来自原生workbuddy Region::Intl；复用纯归一化代码保留随包MIT许可。
mod account;
mod activity;
mod admin;
mod billing;
mod builtin;
mod catalog;
mod chat;
mod desktop;
mod endpoints;
mod errors;
mod headers;
mod image_models;
mod images;
mod journal;
mod login;
mod network;
mod normalize;
mod quota_reset;
mod reasoning;
mod refresh_identity;
mod search;
mod session;
mod util;
mod web_session;

use agent2api_wasm_sdk::{json, Error, Result, Value};
agent2api_wasm_sdk::export_provider!(handle);

fn metadata() -> Value {
    json!({ "id": endpoints::ID, "label": "WorkBuddy 国际版", "order": 16,
        "capabilities": { "chat": true, "stateful": false, "refresh": true, "model_refresh": true, "usage": true,
            "checkin": false, "claims": true, "web_login": true, "sse_model_rewrite": false, "sse_strip_newline_chunks": true,
            "anonymous_default_session": false, "system_proxy_when_unset": false, "refresh_uses_account": true,
            "supports_default_model": true, "env_credentials": false },
        "login_methods": [{ "kind": "browser", "button": "国际站网页登录", "hint": "官方授权后自动轮询，无需本机回调" },
            { "kind": "token", "label": "国际登录态JSON", "placeholder": "粘贴国际客户端.info文件的JSON", "extra_fields": [] },
            { "kind": "desktop", "button": "导入国际本机快照", "hint": "显式导入时只读一次所连接网关电脑的官方国际版固定文件，无需另行授权辅助插件" }],
        "admin_panels": ["daily_keepalive"], "default_models": catalog::fallback()["models"],
        "account_features": { "usage": true, "credit_detail": true, "credit_expiry": true, "edition": false, "identifier": ["uid"], "expiry": ["expiresAt"] },
        "checkin": { "description": "国际每日活跃：条件领取、授权模型保活和网页会话分别核对，不保证次日奖励到账",
            "daily": true, "modes": ["keepalive", "claim", "full"], "growth": false, "onboarding": false, "plans": false } })
}
fn handle(method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()])),
        "provider.info" => Ok(metadata()),
        "ping" => Ok(json!(true)),
        "admin.action" if params["action"] == "auth.methods" => Ok(login::methods(
            params["payload"]["locale"].as_str().unwrap_or("zh-CN"),
        )),
        "admin.action" if params["action"] == "quota.sources" => {
            Ok(billing::sources(&params["payload"]["report"]))
        }
        "admin.action" if params["action"] == "web-search.capabilities" => {
            Ok(search::capabilities())
        }
        "admin.action" if params["action"] == "web-search.search" => {
            search::search(&params["payload"])
        }
        "admin.action" => admin::dispatch(&params),
        "account.normalize" => account::normalize(&params),
        "account.view" => Ok(account::view(&params["secret"])),
        "account.identity" => Ok(json!(account::identity(&params["secret"]))),
        "token.ensure" => Ok(json!(session::ensure(false)?.token())),
        "token.refresh" => Ok(json!(session::ensure(true)?.token())),
        "token.expiring" => Ok(json!(session::is_expiring()?)),
        "login.start" => login::start(&params),
        "login.poll" => login::poll(&params),
        "login.cancel" => login::cancel(&params),
        "models.list" => catalog::list(&params),
        "models.refresh" => catalog::refresh(&params),
        "usage.query" => billing::query(),
        "claim.status" => activity::status(),
        "claim" if matches!(params["claim"].as_str(), None | Some("" | "daily")) => {
            activity::claim_only()
        }
        // 保持原宿主auto_checkin→国际activity.run入口；改成true会跳过authorized_models那层。
        "checkin.claim" => Err(Error::http(400, "国际版需经主程序授权的每日活跃入口执行")
            .with_code("host_authorized_activity_required")),
        // 原Region::Intl没有自建schedules。不能借迁移默认开启第二套定时任务。
        "tasks.list" => Ok(json!([])),
        "tasks.run" => Err(Error::http(
            501,
            "国际日活沿用主程序现有签到排程，不接受缺少授权模型链的任务派发",
        )),
        "conversation" => {
            chat::forward(params)?;
            Ok(Value::Null)
        }
        "images.capabilities" => Ok(json!({ "generation": true, "editing": true })),
        "images.generate" => images::generate(&params, false),
        "images.edit" => images::generate(&params, true),
        "chat.policy" => Ok(json!({ "repairNativeTools": true, "stableConversation": true })),
        "reasoning_patch" => Ok(reasoning::patch(
            params["level"].as_str().unwrap_or(""),
            catalog::known(params["model"].as_str().unwrap_or("")).as_ref(),
            &params["body"],
        )),
        "outbound_reasoning" => Ok(json!(reasoning::outbound_level(
            &params["body"],
            catalog::known(params["body"]["model"].as_str().unwrap_or("")).as_ref()
        ))),
        "classify" => Ok(errors::classify(status(&params), &params["body"])),
        "error.policy" => Ok(errors::policy(status(&params), &params["body"])),
        "retry.decision" => Ok(errors::retry(
            status(&params),
            &params["body"],
            params["attempt"].as_u64().unwrap_or(0) as usize,
            params["budget"].as_u64().unwrap_or(0) as usize,
        )),
        "retry_advice" => Ok(
            if errors::code(&params["body"]) == Some(11128)
                && !errors::channel_rejected(&params["body"])
                && params["attempt"].as_u64().unwrap_or(0) < params["budget"].as_u64().unwrap_or(0)
            {
                json!({ "delay_ms": 10_000, "reason": "上游敏感词拦截（11-128），请检查提示词" })
            } else {
                Value::Null
            },
        ),
        "is_error_response" => Ok(json!(
            !(200..300).contains(&status(&params))
                || params["headers"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .any(|(k, v)| k.eq_ignore_ascii_case("content-type")
                        && v.as_str()
                            .is_some_and(|s| s.to_ascii_lowercase().contains("application/json")))
        )),
        "refusal_markers" => Ok(json!({ "finishes": [], "markers": [] })),
        "cooldown_models" => Ok(json!({ "models": [] })),
        _ => Err(Error::unsupported()),
    }
}
fn status(params: &Value) -> u16 {
    params["status"]
        .as_u64()
        .and_then(|v| u16::try_from(v).ok())
        .unwrap_or(502)
}
