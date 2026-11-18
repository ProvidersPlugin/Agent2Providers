//! 手工 API key、文本 JSON、OAuth 导入与公开投影；本机引用绝不暗读盘。
//! 原生 key/邮箱+渠道身份保持不变，缺邮箱旧空身份改为只在新导入时摘要兜底。

use crate::{
    credentials::{self, Credentials},
    errors, util,
};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn identity(secret: &Value) -> Result<String> {
    credentials::identity(&Credentials::parse(secret)?)
}
pub fn view(secret: &Value) -> Result<Value> {
    let c = Credentials::parse(secret)?;
    let name = if !c.name.is_empty() {
        c.name.clone()
    } else if !c.account_id.is_empty() {
        c.account_id.clone()
    } else if c.api_key() {
        "Kiro API".into()
    } else {
        "Kiro".into()
    };
    let mut value = json!({ "name": name, "accountId": c.account_id, "uid": c.uid,
        "hasCredentials": !c.access.is_empty() || c.local(), "hasRefreshToken": !c.local() && !c.refresh.is_empty(),
        "desktop": c.local(), "canSyncCredentials": c.local(), "canRefresh": c.refreshable(), "available": true });
    if !c.plan.is_empty() {
        value["plan"] = json!(c.plan);
    }
    if let Some(expires) = c.expires {
        value["expiresAt"] = json!(expires);
    }
    if c.api_key() {
        value["longLived"] = json!(true);
    }
    if c.local() {
        value["hasRefreshToken"] = json!(false);
        value["source"] = json!(c.source);
    }
    let mut value = errors::sanitize(&value, &c.secrets());
    let tail = if c.access.chars().count() > 8 {
        c.access
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>()
    } else {
        String::new()
    };
    value["tokenTail"] = json!(if c.local() {
        util::text(secret, &["tokenTail"])
            .chars()
            .take(4)
            .collect::<String>()
    } else if errors::redact(&tail, &c.secrets()) == tail {
        tail
    } else {
        String::new()
    });
    Ok(value)
}
pub fn normalized(mut c: Credentials, name: Option<&str>) -> Result<Value> {
    let name = name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            if !c.name.is_empty() {
                c.name.clone()
            } else if !c.account_id.is_empty() {
                if c.uid.is_empty() {
                    c.account_id.clone()
                } else {
                    format!("{} ({})", c.account_id, c.uid)
                }
            } else if c.api_key() {
                "Kiro API".into()
            } else {
                "Kiro 账号".into()
            }
        });
    c.name = errors::redact(&name, &c.secrets())
        .chars()
        .filter(|ch| !ch.is_control())
        .take(100)
        .collect();
    c.plan = errors::redact(&c.plan, &c.secrets())
        .chars()
        .filter(|ch| !ch.is_control())
        .take(200)
        .collect();
    c.pin_identity();
    let secret = if c.local() {
        json!({"source":c.source,"method":c.method,"accountId":c.account_id,"uid":c.uid,"name":c.name,"region":c.region,
            "loginProvider":c.login_provider,"profileArn":c.profile,"plan":c.plan,"expires":c.expires,"tokenTail":c.access.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect::<String>(),
            "hasRefreshToken":false, "desktopReadOnly":true,
            "desktopFingerprint": util::hash(&c.access, 32)})
    } else {
        c.value()
    };
    Ok(
        json!({ "identity": credentials::identity(&c)?, "public": view(&secret)?, "secret": secret, "desktop": c.local() }),
    )
}
fn own_provider(value: &Value) -> Result<()> {
    if value
        .get("provider")
        .and_then(Value::as_str)
        .is_some_and(|provider| provider != "kiro")
    {
        return Err(Error::http(400, "凭据所属提供商不是Kiro"));
    }
    Ok(())
}
fn from_text(text: &str) -> Result<Value> {
    if text.len() > 128 * 1024 {
        return Err(Error::http(400, "Kiro 凭据文本过大"));
    }
    let text = text.trim();
    if !text.starts_with('{') {
        return Ok(json!({ "apiKey": text, "method": "apikey" }));
    }
    let value: Value =
        serde_json::from_str(text).map_err(|_| Error::http(400, "Kiro 凭据 JSON 格式无效"))?;
    if !value.is_object() {
        return Err(Error::http(400, "Kiro 凭据 JSON 必须为对象"));
    }
    if value.get("secret").is_some_and(Value::is_object) {
        if value
            .get("provider")
            .and_then(Value::as_str)
            .is_some_and(|provider| provider != "kiro")
        {
            return Err(Error::http(400, "凭据所属提供商不是 Kiro"));
        }
        return Ok(value["secret"].clone());
    }
    Ok(value)
}
fn input(params: &Value) -> Result<Value> {
    let source = &params["source"];
    match source["kind"].as_str() {
        Some("desktop") => Err(credentials::local_unavailable()),
        Some("file_text") => from_text(source["content"].as_str().unwrap_or("")),
        Some("manual") => {
            let payload = &source["payload"];
            if let Some(text) = payload.as_str() {
                return from_text(text);
            }
            if !payload.is_object() {
                return Err(Error::http(400, "Kiro 凭据必须是对象或文本"));
            }
            if let Some(index) = payload.get("methodIndex") {
                own_provider(payload)?;
                let index = index
                    .as_u64()
                    .ok_or_else(|| Error::http(400, "Kiro 登录方式无效"))?;
                if !matches!(index, 2 | 3) {
                    return Err(Error::http(400, "请选择 API key 或凭据 JSON 导入"));
                }
                let fields = payload["inputs"]
                    .as_object()
                    .ok_or_else(|| Error::http(400, "Kiro 登录字段必须为对象"))?;
                let allowed: &[&str] = if index == 2 {
                    &["apiKey", "name"]
                } else {
                    &["credentialJson", "name"]
                };
                if fields
                    .iter()
                    .any(|(key, value)| !allowed.contains(&key.as_str()) || !value.is_string())
                {
                    return Err(Error::http(400, "Kiro 登录包含未申报字段"));
                }
                let mut value = if index == 3 {
                    from_text(
                        fields
                            .get("credentialJson")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    )?
                } else {
                    Value::Object(fields.clone())
                };
                if let Some(name) = fields.get("name") {
                    value["name"] = name.clone();
                }
                return Ok(value);
            }
            Ok(payload.clone())
        }
        _ => Err(Error::http(
            400,
            "Kiro 请使用 API key、凭据 JSON 或网页登录",
        )),
    }
}
pub fn normalize(params: &Value) -> Result<Value> {
    if params["source"]["kind"] == "desktop" {
        let mut credentials = crate::local::read()?;
        let profile = crate::usage::fetch(&credentials)?;
        credentials.account_id = crate::usage::verified_email(&profile, &credentials)?;
        credentials.plan = crate::usage::plan_name(
            profile["subscriptionInfo"]["subscriptionTitle"]
                .as_str()
                .unwrap_or(""),
        );
        normalized(credentials, params["name"].as_str())
    } else {
        normalize_manual(params)
    }
}

fn normalize_manual(params: &Value) -> Result<Value> {
    let mut value = input(params)?;
    if value.get("secret").is_some_and(Value::is_object) {
        own_provider(&value)?;
        value = value["secret"].clone();
    }
    for key in ["apiKey", "accessToken", "access", "token", "key"] {
        if let Some(text) = value[key]
            .as_str()
            .filter(|text| text.trim_start().starts_with('{'))
        {
            own_provider(&value)?;
            if value
                .get("source")
                .and_then(Value::as_str)
                .is_some_and(|source| !source.is_empty())
            {
                return Err(credentials::local_unavailable());
            }
            let mut parsed = from_text(text)?;
            if let Some(name) = value.get("name") {
                parsed["name"] = name.clone();
            }
            value = parsed;
            break;
        }
    }
    let c = Credentials::parse(&value)?;
    if c.local() {
        return Err(Error::http(
            400,
            "本机引用请通过导入本机登录态入口添加，不接受手工声明本机来源",
        ));
    }
    let name = util::text(params, &["name"]);
    normalized(c, (!name.is_empty()).then_some(name.as_str()))
}
