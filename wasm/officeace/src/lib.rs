//! OfficeAce 单身份 WASM：完整云端OAuth/DPoP、双层凭据、模型对话、控制面福利。
//! 上游是标准Chat；无独立生图/编辑接口证据，图片仅保留Chat入站的既有透传能力。
//! 模型Basic没有到期机制，STS续期只影响额度/奖励；官方本机文件导入尚未接通。

mod account;
mod activation;
mod activation_state;
mod agreement;
mod chat;
mod checkin;
mod client;
mod control;
mod credentials;
mod dpop;
mod endpoints;
mod errors;
mod identity_claims;
mod ledger;
mod login;
mod login_methods;
mod model_shape;
mod models;
mod oauth_gateway;
mod oauth_tokens;
mod onboarding;
mod probe;
mod refresh;
mod signer;
mod subscription;
mod trial;
mod util;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

sdk::export_provider!(handle);

fn metadata() -> Value {
    json!({ "id": "officeace", "label": "OfficeAce 果办", "order": 12,
        "capabilities": { "chat": true, "stateful": false, "refresh": true, "model_refresh": true,
            "usage": true, "checkin": true, "claims": true, "web_login": true,
            "sse_model_rewrite": false, "sse_strip_newline_chunks": false,
            "anonymous_default_session": false, "system_proxy_when_unset": false,
            "refresh_uses_account": true, "supports_default_model": false, "env_credentials": false },
        "login_methods": [{ "kind": "browser", "button": "网页登录", "hint": "完成华为云授权后自动轮询获取凭据" }],
        "account_features": { "usage": true, "credit_detail": true, "credit_expiry": true,
            "identifier": ["id"], "expiry": ["expiresAt"] },
        "checkin": { "description": "一次结算当天全部奖励（每日签到及新人礼）；套餐需另行确认，不自动续费", "daily": true, "modes": ["checkin"], "onboarding": true },
        "default_models": [] })
}

fn handle(method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()])),
        "provider.info" => Ok(metadata()),
        "ping" => Ok(json!(true)),
        "admin.action" if params["action"] == "auth.methods" => Ok(login_methods::methods(
            params["payload"]["locale"].as_str().unwrap_or("zh-CN"),
        )),
        "admin.action" if params["action"] == "subscription.preview" => activation::preview(),
        "admin.action" if params["action"] == "subscription.ensure" => {
            activation::ensure(params["payload"]["confirmAgreements"] == true)
        }
        "admin.action" if params["action"] == "quota.sources" => {
            Ok(subscription::sources(&params["payload"]))
        }
        "account.normalize" => account::normalize(&params),
        "account.view" => Ok(account::view(&params["secret"])),
        "account.identity" => Ok(json!(account::identity(&params["secret"]))),
        "token.ensure" => {
            account::snapshot()?;
            Ok(json!(""))
        }
        "token.refresh" => Ok(json!(control::Context::refresh()?)),
        "token.expiring" => {
            if params["account"].as_str().unwrap_or("").is_empty() {
                return Ok(json!(false));
            }
            let (record, credential) = account::snapshot()?;
            Ok(json!(
                credentials::can_refresh(&record["secret"])
                    && credential.expiring_within(sdk::expiring_lead(&params, 0))
            ))
        }
        "login.start" => login::start(&params),
        "login.poll" => login::poll(&params),
        "login.callback" => Err(Error::http(400, "OfficeAce 回调由官方云端接收；请完成原授权页面并等待本次自动轮询，不要提交其它回调地址")),
        "login.submit" => login::submit(&params),
        "login.cancel" => login::cancel(&params),
        "models.list" => models::list(&params),
        "models.refresh" => models::refresh(&params),
        "tasks.list" => Ok(probe::tasks()),
        "tasks.categories" => Ok(json!({ "model-probe": "maintenance" })),
        "tasks.run" if params["task"] == "model-probe" => probe::run(&params),
        "conversation" => {
            chat::forward(&params)?;
            Ok(Value::Null)
        }
        "usage.query" => onboarding::usage(),
        "checkin.claim" => checkin::report(),
        "claim.status" => onboarding::status(),
        "claim" => onboarding::claim_all(),
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        "classify" => Ok(errors::classify(
            params["status"]
                .as_u64()
                .and_then(|status| u16::try_from(status).ok())
                .unwrap_or(502),
            &params["body"],
        )),
        "is_error_response" => {
            let status = params["status"].as_u64().unwrap_or(502);
            let json = params["headers"]
                .as_object()
                .into_iter()
                .flatten()
                .any(|(key, value)| {
                    key.eq_ignore_ascii_case("content-type")
                        && value.as_str().is_some_and(|value| {
                            let mime = value
                                .split(';')
                                .next()
                                .unwrap_or("")
                                .trim()
                                .to_ascii_lowercase();
                            mime == "application/json" || mime.ends_with("+json")
                        })
                });
            Ok(json!(!(200..300).contains(&status) || json))
        }
        "refusal_markers" => Ok(json!({ "finishes": ["content_filter"], "markers": [] })),
        "reasoning_patch" => Ok(
            json!({ "action": "skip", "reason": "OfficeAce 不注入 thinking；客户端显式思考设置原样透传" }),
        ),
        "error.policy" | "retry_advice" => Ok(Value::Null),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "cooldown_models" => Ok(json!({ "models": [] })),
        _ => Err(Error::unsupported()),
    }
}
