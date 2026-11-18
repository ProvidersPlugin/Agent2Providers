//! Trae 固定托管回执投影；只核原宿主上下文与公开步骤，不持PKCE/授权码或设备私钥。

use agent2api_wasm_sdk::{self as sdk, json, login::managed, Error, Result, Value};
use std::collections::BTreeSet;

use crate::{i18n, util, PROVIDER_ID};

const ORIGIN: &str = "https://api.trae.cn";

pub fn invalid() -> Error {
    Error::http(
        409,
        "Trae 本次托管登录来源或步骤已失效；未重发、重建回调或改道，请明确重新发起",
    )
    .with_code("trae_managed_login_invalid")
}

pub fn exact(value: &Value, fields: &[&str]) -> Result<()> {
    if value.as_object().is_none_or(|map| {
        map.len() != fields.len() || map.keys().any(|key| !fields.contains(&key.as_str()))
    }) {
        return Err(invalid());
    }
    Ok(())
}

fn id(value: &str) -> bool {
    (16..=256).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

pub fn flow(params: &Value) -> Result<&str> {
    if params["provider"].as_str() != Some(PROVIDER_ID) {
        return Err(invalid());
    }
    params["flow"]
        .as_str()
        .filter(|flow| id(flow) && flow.starts_with("managed_"))
        .ok_or_else(invalid)
}

pub fn locale(params: &Value) -> &str {
    params["payload"]["locale"].as_str().unwrap_or("zh-CN")
}

pub fn context(params: &Value) -> Result<managed::Context> {
    if params.as_object().is_none_or(|fields| {
        fields.keys().any(|key| {
            !matches!(
                key.as_str(),
                "provider"
                    | "flow"
                    | "account"
                    | "method"
                    | "payload"
                    | "interaction"
                    | "proxy"
                    | "proxy_override"
            )
        })
    }) || params["method"].as_str() != Some("browser")
        || params
            .get("account")
            .is_some_and(|value| value.as_str() != Some(""))
    {
        return Err(invalid());
    }
    let context = managed::context()?;
    if context.provider != PROVIDER_ID
        || context.method_index != 0
        || context.flow != flow(params)?
        || !id(&context.interaction)
        || params["interaction"].as_str() != Some(context.interaction.as_str())
        || context.remaining_ms == 0
        || context.remaining_ms > 900_000
        || context.expires_at <= util::now()?
        || context.authorization_origins.len() != 1
        || context.authorization_origins[0] != ORIGIN
    {
        return Err(invalid());
    }
    Ok(context)
}

pub fn recheck(params: &Value, before: &managed::Context) -> Result<()> {
    let after = context(params)?;
    if after.flow != before.flow
        || after.interaction != before.interaction
        || after.expires_at != before.expires_at
        || after.remaining_ms > before.remaining_ms
    {
        return Err(invalid());
    }
    Ok(())
}

pub fn variant(params: &Value, context: &managed::Context) -> Result<&'static str> {
    let payload = &params["payload"];
    if payload.as_object().is_none_or(|fields| {
        fields.keys().any(|key| {
            !matches!(
                key.as_str(),
                "methodIndex" | "inputs" | "mode" | "locale" | "proxy"
            )
        })
    }) || payload["methodIndex"].as_u64() != Some(context.method_index)
        || payload
            .get("mode")
            .is_some_and(|value| !matches!(value.as_str(), Some("external" | "auto")))
    {
        return Err(invalid());
    }
    // proxy仅为宿主原传输信封；出口由原Call私有持有，本插件不读/改/传给上游。
    let empty = json!({});
    let input = payload.get("inputs").unwrap_or(&empty);
    if input
        .as_object()
        .is_none_or(|fields| fields.keys().any(|key| key != "variant"))
    {
        return Err(invalid());
    }
    match input.get("variant") {
        None => Ok("solo"), // auth.methods的明确国内默认；显式未知/intl绝不回落。
        Some(Value::String(value)) if value == "solo" => Ok("solo"),
        Some(Value::String(value)) if value == "cn" => Ok("cn"),
        _ => Err(Error::http(
            400,
            "Trae 本次登录只接受明确的 solo 或 cn；未知或国际版不会改成国内版",
        )
        .with_code("trae_managed_variant_invalid")),
    }
}

pub struct Prepared {
    lease_id: String,
    authorization_url: String,
    expires_at: i64,
}

fn authorization(raw: &str) -> Result<()> {
    if raw.is_empty()
        || raw.len() > 16 * 1024
        || raw
            .chars()
            .any(|value| value.is_whitespace() || value.is_control())
        || raw.contains('\\')
    {
        return Err(invalid());
    }
    let url = url::Url::parse(raw).map_err(|_| invalid())?;
    if url.origin().ascii_serialization() != ORIGIN
        || url.scheme() != "https"
        || url.path() != "/authorization"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !raw.starts_with("https://api.trae.cn/authorization?")
    {
        return Err(invalid());
    }
    let mut keys = BTreeSet::new();
    for (key, value) in url.query_pairs() {
        if key.is_empty() || !keys.insert(key.into_owned()) || value.chars().any(char::is_control) {
            return Err(invalid());
        }
    }
    Ok(())
}

pub fn prepare(context: &managed::Context, variant: &str) -> Result<Prepared> {
    let value = sdk::host(
        "login.trae.context.prepare",
        json!({ "expected": context.interaction, "variant": variant }),
    )?;
    exact(&value, &["leaseId", "authorizationUrl", "expiresAt"])?;
    let lease_id = value["leaseId"]
        .as_str()
        .filter(|value| id(value))
        .ok_or_else(invalid)?
        .to_string();
    let authorization_url = value["authorizationUrl"]
        .as_str()
        .ok_or_else(invalid)?
        .to_string();
    let expires_at = value["expiresAt"].as_i64().ok_or_else(invalid)?;
    if expires_at != context.expires_at
        || lease_id == context.interaction
        || lease_id == context.flow
    {
        return Err(invalid());
    }
    authorization(&authorization_url)?;
    Ok(Prepared {
        lease_id,
        authorization_url,
        expires_at,
    })
}

pub fn publish(context: &managed::Context, prepared: &Prepared, locale: &str) -> Result<Value> {
    // SDK通用publish_browser默认手填=true，不适用于Trae无state原监听回调。
    let step = sdk::host(
        "login.step.publish",
        json!({ "expected": context.interaction, "title": i18n::web_title(locale), "message": i18n::web(locale),
            "captcha": null, "browser": { "url": prepared.authorization_url, "leaseId": prepared.lease_id,
                "browserMode": "external", "manualCallback": false, "callbackInput": "url" } }),
    )?;
    exact(
        &step,
        &[
            "type",
            "id",
            "provider",
            "flow",
            "methodIndex",
            "status",
            "title",
            "message",
            "fields",
            "expiresAt",
            "remainingMs",
            "captcha",
            "browser",
        ],
    )?;
    if step["type"] != "managed"
        || step["id"]
            .as_str()
            .is_none_or(|value| !id(value) || value == context.interaction)
        || step["provider"] != context.provider
        || step["flow"] != context.flow
        || step["methodIndex"].as_u64() != Some(0)
        || step["status"] != "ready"
        || step["expiresAt"].as_i64() != Some(prepared.expires_at)
        || step["remainingMs"]
            .as_u64()
            .is_none_or(|left| left == 0 || left > context.remaining_ms)
        || step["fields"]
            .as_array()
            .is_none_or(|fields| !fields.is_empty())
        || !step["captcha"].is_null()
        || step["title"]
            .as_str()
            .is_none_or(|value| value.is_empty() || value.len() > 256)
        || step["message"]
            .as_str()
            .is_none_or(|value| value.len() > 8192)
    {
        return Err(invalid());
    }
    exact(
        &step["browser"],
        &["url", "browserMode", "manualCallback", "callbackInput"],
    )?;
    if step["browser"]["url"] != prepared.authorization_url
        || step["browser"]["browserMode"] != "external"
        || step["browser"]["manualCallback"] != false
        || step["browser"]["callbackInput"] != "url"
    {
        return Err(invalid());
    }
    Ok(step)
}

/// 只检查宿主账号外形后按值移交原account；不归一化、重建secret/public或请求资料。
pub fn account(mut reply: Value) -> Result<Value> {
    exact(&reply, &["account"])?;
    let account = reply
        .as_object_mut()
        .and_then(|reply| reply.remove("account"))
        .ok_or_else(invalid)?;
    exact(&account, &["identity", "public", "secret", "desktop"])?;
    if account["desktop"] != false
        || account["identity"].as_str().is_none_or(|value| {
            value.is_empty()
                || value.len() > 512
                || value.trim() != value
                || value.chars().any(char::is_control)
        })
        || !account["public"].is_object()
        || !account["secret"].is_object()
    {
        return Err(invalid());
    }
    Ok(account)
}
