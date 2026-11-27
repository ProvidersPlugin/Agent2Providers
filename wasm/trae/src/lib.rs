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
            class("unsupported", &[400], &["trae_route_capability"]),
        ],
        vec![
            rule("trae_plan", "套餐或模型额度耗尽", "当前套餐或模型额度不足时冷却该对象并切换可用账号。", "quota", &[], "trae_plan_limit", "switch_account", "fixed", 1800, true, 7200, "model"),
            rule("trae_route_capability", "当前模型能力不支持", "当前账号目录下该模型不支持所需能力（如图片输入）；允许尝试其他兼容提供商，但不对 Trae 整家降权或冷却。", "unsupported", &[400], "trae_route_capability", "switch_provider", "none", 60, false, 60, "model"),
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
            } else {
                Ok(reasoning_patch(&params))
            }
        }
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        _ => Err(Error::unsupported()),
    }
}

fn reasoning_patch(params: &Value) -> Value {
    let level = params["level"].as_str().unwrap_or("").trim();
    if level.is_empty()
        || matches!(
            level.to_ascii_lowercase().as_str(),
            "off" | "none" | "disabled"
        )
    {
        return json!({ "action": "skip", "reason": "Trae 映射绑定不注入关闭思考" });
    }
    let model = params["model"].as_str().unwrap_or("").trim();
    if !model.is_empty() {
        if let Ok(cached) = sdk::catalog::cached() {
            if let Some(entry) = cached["models"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|item| {
                    item["id"]
                        .as_str()
                        .is_some_and(|id| id.eq_ignore_ascii_case(model))
                })
            {
                if let Some(raw_levels) = entry["reasoningEffortLevels"].as_array() {
                    let levels: Vec<String> = raw_levels
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect();
                    return match catalog::resolve_reasoning_effort(&levels, level) {
                        Some(resolved) => {
                            json!({ "action": "set", "field": "reasoning_effort", "value": resolved })
                        }
                        None => {
                            json!({ "action": "skip", "reason": "Trae 当前模型不支持所选思考档位" })
                        }
                    };
                }
                if entry["supportsReasoning"] == false {
                    return json!({ "action": "skip", "reason": "Trae 当前模型不支持思考档位" });
                }
            }
        }
    }
    if matches!(
        level.to_ascii_lowercase().as_str(),
        "minimal" | "low" | "light" | "medium" | "high" | "xhigh" | "extra_high" | "max"
    ) {
        // 缓存未命中时放行已知正向档位，最终在 payload 中按本账号真实路由对齐或丢弃。
        json!({ "action": "set", "field": "reasoning_effort", "value": level })
    } else {
        json!({ "action": "skip", "reason": "Trae 当前思考档位未获目录支持" })
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
