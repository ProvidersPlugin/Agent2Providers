use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::login;

pub fn methods() -> Value {
    json!({ "methods": [
        { "methodIndex": 0, "type": "sms", "label": "手机验证码登录", "prompts": [], "browserMode": "none",
          "sms": { "phoneLabel": "手机号", "phonePlaceholder": "11 位大陆手机号",
            "countryCodes": [{ "value": "86", "label": "+86" }], "phoneMinLength": 11, "phoneMaxLength": 11,
            "codeMinLength": 6, "codeMaxLength": 6, "resendAfterSeconds": 60, "expiresAfterSeconds": 300 } },
        { "methodIndex": 1, "type": "api", "label": "填写凭证", "prompts": [
            { "key": "session", "type": "text", "label": "Session", "secret": true },
            { "key": "phone", "type": "text", "label": "手机号", "optional": true },
            { "key": "userId", "type": "text", "label": "用户 ID", "optional": true }
          ] }
    ] })
}

pub async fn send(ctx: &PluginCtx, payload: &Value) -> Result<Value, String> {
    check_method(payload)?;
    let phone = text(payload, "phone");
    let proxy = proxy(ctx, payload)?;
    let ticket = login::send_code(&proxy, phone).await?;
    Ok(json!({ "status": "sent", "ticket": ticket, "retryAfterSeconds": 60, "expiresAfterSeconds": 300 }))
}

pub async fn verify(ctx: &PluginCtx, payload: &Value) -> Result<Value, String> {
    check_method(payload)?;
    let proxy = proxy(ctx, payload)?;
    let account = login::login_with_code(&proxy, text(payload, "phone"), text(payload, "code"),
        payload["ticket"]["msgid"].as_str(), None).await?;
    serde_json::to_value(account).map_err(|_| "登录结果无法序列化".to_string())
}

fn proxy(ctx: &PluginCtx, payload: &Value) -> Result<String, String> {
    match payload.get("proxy") {
        None => Ok(ctx.env.proxy.clone()),
        Some(Value::Null) => Ok("direct".to_string()),
        Some(value) => serde_json::from_value::<agent2api_provider_protocol::WireProxy>(value.clone())
            .map(|proxy| agent2api_provider_sdk::egress::proxy_url(&proxy))
            .map_err(|_| "短信登录代理无效".to_string()),
    }
}

fn check_method(payload: &Value) -> Result<(), String> {
    if payload["methodIndex"].as_u64() != Some(0) || payload["countryCode"] != "86" {
        return Err(agent2api_provider_sdk::control_error(400, "Loomy 短信登录方式或手机区号无效", None, None));
    }
    Ok(())
}

pub fn account_input(mut input: agent2api_provider_protocol::NormalizeInput) -> Result<agent2api_provider_protocol::NormalizeInput, String> {
    if let agent2api_provider_protocol::NormalizeSource::Manual { payload } = &mut input.source {
        if payload.get("methodIndex").is_some() {
            if payload["methodIndex"].as_u64() != Some(1) {
                return Err("此登录方式不能用于手动填写凭证".into());
            }
            let fields = payload["inputs"].as_object().filter(|fields| fields.iter().all(|(key, value)|
                matches!(key.as_str(), "session" | "phone" | "userId") && value.is_string()))
                .ok_or("Loomy 凭证输入无效")?;
            if fields.get("session").and_then(Value::as_str).is_none_or(|value| value.trim().is_empty()) {
                return Err("请填写 Session".into());
            }
            *payload = Value::Object(fields.clone());
        }
    }
    Ok(input)
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str { value[key].as_str().unwrap_or("") }
