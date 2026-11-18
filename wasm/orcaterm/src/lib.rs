//! OrcaTerm 固定账号与请求内会话；当前目录/图像合同对照官方 1.6.6 的公开适配证据。
//! 包只调用受控宿主服务，模型与地区身份、用户执行的工具和一次性授权分别隔离。
//! 微信回调映射和换码结果存受控登录状态；换码已发出而结果未知时不盲目重放。

mod account;
mod balance;
mod chat;
mod credentials;
mod errors;
mod images;
mod login;
mod login_methods;
mod models;
mod prompt;
mod stream;
mod upstream;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

sdk::export_provider!(handle);

pub const ID: &str = "orcaterm";
pub const CGI_BASE: &str = "https://api.orcaterm.cloud.tencent.com";
pub const LIGHTAI_BASE: &str = "https://lightai.cloud.tencent.com";
pub const WEB_BASE: &str = "https://orcaterm.com";
pub const DESKTOP_HELPER: &str = "desktop.credentials.orcaterm.v1";
pub const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 ",
    "(KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36"
);

fn metadata() -> Value {
    json!({
        "id": ID, "label": "OrcaTerm", "order": 4,
        "capabilities": {
            "chat": true, "stateful": true, "refresh": true, "model_refresh": true,
            "usage": true, "checkin": false, "claims": false, "web_login": true,
            "sse_model_rewrite": false, "sse_strip_newline_chunks": false,
            "anonymous_default_session": false, "system_proxy_when_unset": false,
            "refresh_uses_account": true, "supports_default_model": false, "env_credentials": false
        },
        "login_methods": [
            { "kind": "token", "label": "accessToken", "placeholder": "粘贴 OrcaTerm 的 JWT，或整个 data.bin 的内容", "extra_fields": [] },
            { "kind": "browser", "button": "微信扫码登录", "hint": "在官方授权页扫码，应用自动核对结果，无需复制回调" },
            { "kind": "desktop", "button": "导入本机登录态", "hint": "只读 OrcaTerm 官方客户端当前登录文件，无需安装或授权辅助插件" }
        ],
        "account_features": { "usage": true, "identifier": ["uid"], "expiry": ["tokenExpireAt"] },
        "default_models": models::list()
    })
}

fn handle(method: &str, params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()])),
        "provider.info" => Ok(metadata()),
        "ping" => Ok(json!(true)),
        "admin.action" if params["action"] == "auth.methods" => Ok(login_methods::methods(
            params["payload"]["locale"].as_str().unwrap_or("zh-CN"),
        )),
        "account.normalize" => account::normalize(&params),
        "account.view" => account::view(&params["secret"]),
        "account.identity" => Ok(json!(account::identity(&params["secret"])?)),
        "token.ensure" => Ok(json!(account::ensure()?.access_token)),
        "token.refresh" => Ok(json!(account::refresh()?.access_token)),
        "token.expiring" => Ok(json!(account::snapshot()
            .map(|value| value.is_expiring())
            .unwrap_or(false))),
        "models.list" => models::snapshot(),
        "models.refresh" => models::refresh(params["force"] == true),
        "usage.query" => balance::query(),
        "login.start" => login::start(&params),
        "login.poll" => login::poll(),
        "login.callback" => login::callback(&params),
        "login.cancel" => login::cancel(),
        "conversation" => {
            chat::forward(&params)?;
            Ok(Value::Null)
        }
        "classify" => Ok(errors::classify(&params)),
        "is_error_response" => Ok(json!(
            params["status"].as_u64().unwrap_or(502) >= 400
                || errors::business_failed(&params["body"])
        )),
        "error.policy" => Ok(Value::Null),
        "retry.decision" => Ok(json!({ "action": "default" })),
        "retry_advice" => Ok(Value::Null),
        "refusal_markers" => Ok(json!({ "finishes": [], "markers": [] })),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "reasoning_patch" => Ok(
            json!({ "action": "skip", "reason": "OrcaTerm 由当前账号模型目录映射思考参数，不套用其他提供商字段" }),
        ),
        "images.capabilities" => Ok(json!({ "generation": false, "editing": false })),
        _ => Err(Error::unsupported()),
    }
}
