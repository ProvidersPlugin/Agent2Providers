//! 独立Trae国内WASM。私有信封/目录/活动留在本包，无WASI与越权宿主旁路。
//! 方式0消费宿主原监听/一次换码/身份见证，系统浏览器无手填；真实账号仍未验收。
//! 手工导入与旧账号保持；claims没有新手端点证据。

mod account;
mod catalog;
mod chat;
mod checkin;
mod client;
mod credentials;
mod device;
mod endpoints;
mod errors;
mod events;
mod headers;
mod i18n;
mod login;
mod login_stage;
mod models;
mod payload;
mod profile;
mod refresh;
mod stream;
mod usage;
mod usage_packs;
mod util;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
pub const PROVIDER_ID: &str = "trae";
sdk::export_provider!(handle);

fn metadata() -> Value {
    use sdk::error_rules::{attach, class, rule};
    let meta = json!({ "id": PROVIDER_ID, "label": "Trae", "order": 26,
        "capabilities": { "chat": true, "stateful": true, "refresh": true, "model_refresh": true,
            "usage": true, "checkin": true, "claims": false, "web_login": true, "sse_model_rewrite": false,
            "refresh_uses_account": true, "supports_default_model": false, "env_credentials": false,
            "anonymous_default_session": false },
        "login_methods": [ { "kind": "browser", "button": "托管网页登录", "hint": i18n::WEB },
            { "kind": "token", "label": "登录凭据", "placeholder": "accessToken或完整凭据JSON", "extra_fields": [] } ],
        "account_features": { "usage": true, "credit_detail": true, "credit_expiry": true,
            "identifier": ["uid"], "expiry": ["expiresAt", "expiresAtMs"], "long_lived": false },
        "checkin": { "description": "Trae每日积分签到，领取后用账号原设备回查确认；被拒绝或未确认时不记成功", "daily": true, "modes": ["checkin"] },
        "refusal_finishes": ["content_filter"], "refusal_markers": [],
        "default_models": [] });
    attach(
        meta,
        vec![
            class("quota", &[429], &["trae_plan_limit"]),
            class("rate_limit", &[429], &["trae_rate_limited"]),
            class("auth", &[401], &["trae_session_expired"]),
            class("content", &[], &["trae_content_blocked"]),
        ],
        vec![
            rule("trae_plan", "套餐或模型额度耗尽", "当前套餐或模型额度不足时冷却该对象并切换可用账号。", "quota", &[], "trae_plan_limit", "switch_account", "fixed", 1800, true, 7200, "model"),
        ],
    )
}

fn handle(method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()])),
        "provider.info" => Ok(metadata()),
        "ping" => Ok(json!(true)),
        "admin.action" if params["action"] == "auth.methods" => Ok(i18n::methods(
            params["payload"]["locale"].as_str().unwrap_or("zh-CN"),
        )),
        "admin.action" if params["action"] == "quota.sources" => Ok(usage::sources(
            params["payload"]
                .get("report")
                .unwrap_or(&params["payload"]),
        )),
        "account.normalize" => account::normalize(&params),
        "account.view" => Ok(account::view(&credentials::parse(&params["secret"])?)),
        "account.identity" => Ok(json!(account::identity(&credentials::parse(
            &params["secret"]
        )?))),
        "token.ensure" | "token.refresh" => {
            let record = refresh::ensure(method == "token.refresh", None)?;
            Ok(json!(credentials::token(&credentials::parse(
                &record["secret"]
            )?)?))
        }
        "token.expiring" => Ok(json!(refresh::expiring(sdk::expiring_lead(&params, 0))?)),
        "models.list" => models::list(&params),
        "models.refresh" => models::refresh(&params),
        "usage.query" => usage::query(),
        "checkin.claim" => checkin::claim(&params),
        "login.start" => login::start(&params),
        "login.callback" => login::callback(&params),
        "login.poll" => login::poll(&params),
        "login.cancel" => login::cancel(&params),
        "conversation" => {
            chat::forward(params)?;
            Ok(Value::Null)
        }
        "classify" if login_stopped(&params["body"]) => Ok(json!({ "kind": "fatal", "status": 502,
            "message": "Trae 本轮授权或身份结果未确认，禁止重兑授权码；请明确重新发起登录",
            "upstream_code": errors::code(&params["body"]), "reset_at": null })),
        "classify" => Ok(errors::classify(
            params["status"].as_u64().unwrap_or(502) as u16,
            &params["body"],
        )),
        "is_error_response" => Ok(json!(
            params["status"].as_u64().unwrap_or(502) >= 400 || errors::failed(&params["body"])
        )),
        "error.policy" => Ok(
            json!({ "stop": params["body"]["providerCode"] == "local_precondition"
                || login_stopped(&params["body"]) }),
        ),
        "retry.decision" if login_stopped(&params["body"]) => Ok(json!({ "action": "stop",
            "reason": "Trae 一次授权码或身份结果未确认，不自动重发，请明确重新授权" })),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "refusal_markers" => Ok(json!({ "finishes": ["content_filter"], "markers": [] })),
        "reasoning_patch" => {
            if params["body"].get("reasoning_effort").is_some()
                || params["body"].pointer("/reasoning/effort").is_some()
            {
                Ok(json!({ "action": "skip", "reason": "客户端已指定思考档位" }))
            } else if params["level"].as_str().is_some_and(|level| {
                matches!(
                    level,
                    "low" | "light" | "medium" | "high" | "xhigh" | "extra_high" | "max"
                )
            }) {
                // 实际档位在payload中按当前账号route映射，不能按模型名称猜。
                Ok(
                    json!({ "action": "set", "field": "reasoning_effort", "value": params["level"] }),
                )
            } else {
                Ok(json!({ "action": "skip", "reason": "Trae 当前思考档位未获目录支持" }))
            }
        }
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        _ => Err(Error::unsupported()),
    }
}

// 登录专属停止标记先于401/限额分类，不改变普通刷新或聊天的既有协议。
fn login_stopped(body: &Value) -> bool {
    ["providerCode", "code"].iter().any(|field| {
        matches!(
            body[*field].as_str(),
            Some(
                "trae_oauth_code_exchange_stopped"
                    | "trae_oauth_identity_unconfirmed"
                    | "trae_managed_login_invalid"
                    | "trae_managed_variant_invalid"
            )
        )
    }) || ["/error/providerCode", "/error/code"].iter().any(|path| {
        matches!(
            body.pointer(path).and_then(Value::as_str),
            Some(
                "trae_oauth_code_exchange_stopped"
                    | "trae_oauth_identity_unconfirmed"
                    | "trae_managed_login_invalid"
                    | "trae_managed_variant_invalid"
            )
        )
    })
}
