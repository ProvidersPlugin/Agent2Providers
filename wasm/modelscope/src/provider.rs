//! ModelScope 两地区的共享实现，来源为 crates/providers/src/providers/modelscope。
//! 只有编译期协议代码共用；每个 cdylib 入口固定一个地区，宿主按包/账号隔离目录。
//! ms- 令牌既作 Bearer 调推理，也作 m_session_id 调对应网页站。沿用原实现：
//! 长期令牌没有续期接口；领取只有登录触发的每日魔粒，没有额外领取入口。

#[path = "provider/account.rs"]
mod account;
#[path = "provider/balance.rs"]
mod balance;
#[path = "provider/chat.rs"]
mod chat;
#[path = "provider/checkin.rs"]
mod checkin;
#[path = "provider/client.rs"]
mod client;
#[path = "provider/clock.rs"]
mod clock;
#[path = "provider/errors.rs"]
mod errors;
#[path = "provider/images.rs"]
mod images;
#[path = "provider/login.rs"]
mod login;
#[path = "provider/models.rs"]
mod models;
#[path = "provider/region.rs"]
mod region;
#[path = "provider/transactions.rs"]
mod transactions;

pub use region::Region;

use agent2api_wasm_sdk::{json, Error, Result, Value};

fn metadata(region: Region) -> Value {
    json!({
        "id": region.provider_id(), "label": format!("ModelScope {}", region.label()),
        "order": region.order(),
        "capabilities": {
            "chat": true, "stateful": false, "refresh": false, "model_refresh": true,
            "usage": true, "checkin": true, "claims": false, "web_login": false,
            "sse_model_rewrite": false, "sse_strip_newline_chunks": false,
            "anonymous_default_session": false, "system_proxy_when_unset": false,
            "refresh_uses_account": true, "supports_default_model": false, "env_credentials": false
        },
        "login_methods": [{
            "kind": "token", "label": "ms- 令牌",
            "placeholder": "粘贴 ms- 开头的令牌", "extra_fields": []
        }],
        "account_features": {
            "usage": true, "credit_detail": true, "identifier": ["userId"], "long_lived": true
        },
        "checkin": {
            "description": match region {
                Region::Cn => "无独立签到接口 · 每天读取登录信息触发魔粒，以当日发放流水确认",
                Region::Intl => "与国内版同一签到链路 · 站点不同、令牌不通用，以当日发放流水确认"
            },
            "modes": ["checkin"], "daily": true
        },
        "refusal_finishes": ["content_filter", "safety"], "refusal_markers": [],
        "default_models": []
    })
}

pub fn handle(region: Region, method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata(region)])),
        "ping" => Ok(json!(true)),
        "provider.info" => Ok(metadata(region)),
        "admin.action" if params["action"] == "auth.methods" => Ok(login::methods(
            region,
            params["payload"]["locale"].as_str().unwrap_or("zh-CN"),
        )),
        "account.normalize" => account::normalize(region, &params),
        "account.view" => Ok(account::view(region, &params["secret"])),
        "account.identity" => Ok(json!(account::identity(region, &params["secret"])?)),
        // 与原生默认 refresh_token 一致，只回读同一长期令牌，不伪造续期。
        "token.ensure" | "token.refresh" => Ok(json!(account::ensure()?)),
        "token.expiring" => Ok(json!(false)),
        "models.list" => models::list(&params),
        "models.refresh" => models::refresh(region, &params),
        "usage.query" => balance::query(region),
        "checkin.claim" => checkin::claim(region),
        "conversation" => {
            chat::forward(region, &params)?;
            Ok(Value::Null)
        }
        "images.capabilities" => Ok(json!({ "generation": true, "editing": true })),
        "images.generate" => images::generate(region, &params, false),
        "images.edit" => images::generate(region, &params, true),
        "classify" => Ok(errors::classify(
            region,
            errors::status(&params),
            &params["body"],
        )),
        "is_error_response" => Ok(json!(
            errors::status(&params) >= 400 || !client::succeeded(&params["body"])
        )),
        "error.policy" => Ok(Value::Null),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "refusal_markers" => Ok(json!({ "finishes": ["content_filter", "safety"], "markers": [] })),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "reasoning_patch" => Ok(json!({
            "action": "skip", "reason": "上游目录不提供思考能力位，按原始请求透传，不按模型名猜测"
        })),
        _ => Err(Error::unsupported()),
    }
}
