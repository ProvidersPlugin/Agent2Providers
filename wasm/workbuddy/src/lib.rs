//! WorkBuddy国内单身份WASM；所有副作用经宿主，实例可逐次重建。
//! 部分归一化和成长协议迁自现有原生实现，保留Sliverkiss/WorkBuddy2API-Hub/Magpie MIT许可。
mod account;
mod admin;
mod billing;
mod builtin;
mod catalog;
mod chat;
mod desktop;
mod endpoints;
mod errors;
mod growth;
mod headers;
mod image_models;
mod images;
mod ledger;
mod login;
mod network;
mod normalize;
mod quota_reset;
mod reasoning;
mod refresh_identity;
mod search;
mod session;
mod tasks;
mod util;
mod web_session;

use agent2api_wasm_sdk::{json, Error, Result, Value};
agent2api_wasm_sdk::export_provider!(handle);

fn metadata() -> Value {
    use agent2api_wasm_sdk::error_rules::{attach, class, rule};
    let meta = json!({ "id": endpoints::ID, "label": "WorkBuddy 国内版", "order": 15,
        "capabilities": { "chat": true, "stateful": false, "refresh": true, "model_refresh": true, "usage": true,
            "checkin": true, "claims": true, "web_login": true, "sse_model_rewrite": false, "sse_strip_newline_chunks": true,
            "anonymous_default_session": false, "system_proxy_when_unset": false, "refresh_uses_account": true,
            "supports_default_model": true, "env_credentials": false },
        "login_methods": [{ "kind": "browser", "button": "网页登录", "hint": "官方页面授权后自动轮询，不需要回调" },
            { "kind": "token", "label": "登录态JSON", "placeholder": "粘贴官方.info文件的JSON内容", "extra_fields": [] },
            { "kind": "desktop", "button": "导入本机登录快照", "hint": "只读本家固定登录文件，不需额外授权；加密文件请改用网页登录，导入后不依赖客户端文件" }],
        "admin_panels": ["growth_tasks"], "default_models": catalog::fallback()["models"],
        "account_features": { "usage": true, "credit_detail": true, "credit_expiry": true, "edition": true, "identifier": ["uid"], "expiry": ["expiresAt"] },
        "checkin": { "description": "国内个人账号每日签到；成长活跃和连登奖励独立核对", "daily": true, "modes": ["checkin"], "growth": true } });
    attach(
        meta,
        vec![
            class("transient", &[], &["14003", "6001", "6002", "6005", "6006"]),
            class("quota", &[], &["14001", "14012", "14013", "14014", "14015", "14016", "14017", "14018", "14019", "10105"]),
            class("auth", &[401], &["12153", "11140"]),
            class("content", &[], &["11128", "workbuddy_content_blocked"]),
            class("bad_request", &[400, 413, 422], &["11115", "11101", "11133", "11158"]),
        ],
        vec![
            rule("wb_restricted", "账号受限（11140）", "上游标记账号存在异常行为时，将该账号长期冷却并立即切换其他可用账号。", "auth", &[], "11140", "switch_account", "fixed", 3600, true, 86400, "account"),
            rule("wb_content_11128", "敏感词拦截（11128）", "提示词触发上游敏感词检查时，将当前对象降级并切换可用候选。", "content", &[], "11128", "switch_provider", "deprioritize", 600, false, 600, "model"),
        ],
    )
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
        "token.expiring" => Ok(json!(session::is_expiring(agent2api_wasm_sdk::expiring_lead(&params, 0))?)),
        "login.start" => login::start(&params),
        "login.poll" => login::poll(&params),
        "login.cancel" => login::cancel(&params),
        "models.list" => catalog::list(&params),
        "models.refresh" => catalog::refresh(&params),
        "usage.query" => billing::query(),
        "checkin.claim" => billing::checkin::claim(),
        "claim.status" => billing::rewards::status(),
        "claim" => match params["claim"].as_str().unwrap_or("") {
            "" => billing::rewards::claim_all(),
            kind => billing::rewards::claim(kind),
        },
        "tasks.list" => Ok(tasks::list()),
        "tasks.categories" => {
            Ok(json!({ "travel": "growth", "growth-daily": "growth", "growth-streak": "rewards" }))
        }
        "tasks.run" => tasks::run(&params),
        "conversation" => {
            chat::forward(params)?;
            Ok(Value::Null)
        }
        "images.capabilities" => Ok(json!({ "generation": true, "editing": true })),
        "images.generate" => images::generate(&params, false),
        "images.edit" => images::generate(&params, true),
        "chat.policy" => Ok(
            json!({ "repairNativeTools": true, "stableConversation": true, "network": endpoints::network_policy() }),
        ),
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
