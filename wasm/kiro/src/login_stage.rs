//! 冻结的 Kiro 托管阶段回执；这里只核对宿主真实结果，不开监听或自选端口。
//! typed SDK Loopback 只覆盖通用三字段，所以本家附加合同经原 sdk::host 读取。

use crate::{endpoints, util};
use agent2api_wasm_sdk::{self as sdk, json, login::managed, Error, Result, Value};
use std::collections::HashMap;

pub const PORTAL_PROFILE: &str = "kiro.portal.v1";
pub const AWS_PROFILE: &str = "kiro.aws.v1";
fn origins() -> Vec<&'static str> {
    std::iter::once(endpoints::PORTAL)
        .chain(sdk::aws_oidc::ORIGINS.iter().copied()).collect()
}

pub struct Stage {
    pub lease_id: String,
    pub callback_url: String,
    pub callback_urls: Vec<String>,
    pub redirect_uri: String,
    pub port: u16,
    pub oauth_state: String,
    pub expires_at: i64,
}

pub fn invalid() -> Error {
    Error::http(
        409,
        "Kiro托管授权材料已失效或阶段不一致，未重发授权码，请重新发起",
    )
    .with_code("kiro_managed_invalid")
}
pub fn exact(value: &Value, fields: &[&str]) -> Result<()> {
    if value.as_object().is_none_or(|map| {
        map.len() != fields.len() || map.keys().any(|key| !fields.contains(&key.as_str()))
    }) {
        return Err(invalid());
    }
    Ok(())
}
fn text(value: &Value, key: &str, maximum: usize) -> Result<String> {
    value[key]
        .as_str()
        .filter(|value| {
            !value.is_empty()
                && value.len() <= maximum
                && !value
                    .bytes()
                    .any(|byte| byte <= b' ' || byte == 127 || byte == b'\\')
        })
        .map(str::to_string)
        .ok_or_else(invalid)
}
fn nonce(value: &str) -> bool {
    (16..=256).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

pub fn context(params: &Value) -> Result<managed::Context> {
    let context = managed::context()?;
    if context.provider != endpoints::ID
        || context.method_index != 0
        || params["provider"].as_str() != Some(endpoints::ID)
        || params["flow"].as_str() != Some(context.flow.as_str())
        || params["interaction"].as_str() != Some(context.interaction.as_str())
        || !nonce(&context.interaction)
        || !nonce(&context.flow)
        || !context.flow.starts_with("managed_")
        || context.remaining_ms == 0
        || context.expires_at <= util::now()?
        || context.authorization_origins.len() != origins().len()
        || origins().iter().any(|origin| {
            context
                .authorization_origins
                .iter()
                .filter(|value| value.as_str() == *origin)
                .count()
                != 1
        })
    {
        return Err(invalid());
    }
    Ok(context)
}

pub fn fresh(before: &managed::Context) -> Result<managed::Context> {
    let params =
        json!({"provider":before.provider, "flow":before.flow, "interaction":before.interaction});
    let after = context(&params)?;
    if after.expires_at != before.expires_at || after.remaining_ms > before.remaining_ms {
        return Err(invalid());
    }
    Ok(after)
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

impl Stage {
    fn parse(value: Value, profile: &str, context: &managed::Context) -> Result<Self> {
        exact(
            &value,
            &[
                "leaseId",
                "callbackUrl",
                "callbackUrls",
                "redirectUri",
                "port",
                "oauthState",
                "expiresAt",
            ],
        )?;
        let stage = Self {
            lease_id: text(&value, "leaseId", 256)?,
            callback_url: text(&value, "callbackUrl", 256)?,
            callback_urls: value["callbackUrls"]
                .as_array()
                .filter(|values| values.len() <= 2)
                .ok_or_else(invalid)?
                .iter()
                .map(|value| value.as_str().map(str::to_string).ok_or_else(invalid))
                .collect::<Result<_>>()?,
            redirect_uri: text(&value, "redirectUri", 256)?,
            port: value["port"]
                .as_u64()
                .and_then(|port| u16::try_from(port).ok())
                .filter(|port| endpoints::PORTS.contains(port))
                .ok_or_else(invalid)?,
            oauth_state: text(&value, "oauthState", 256)?,
            expires_at: value["expiresAt"].as_i64().ok_or_else(invalid)?,
        };
        if !nonce(&stage.lease_id)
            || !nonce(&stage.oauth_state)
            || stage.oauth_state == stage.lease_id
            || stage.oauth_state == context.flow
            || stage.expires_at != context.expires_at
        {
            return Err(invalid());
        }
        let expected = match profile {
            PORTAL_PROFILE => vec![
                format!("http://localhost:{}/signin/callback", stage.port),
                format!("http://localhost:{}/oauth/callback", stage.port),
            ],
            AWS_PROFILE => vec![format!("http://127.0.0.1:{}/oauth/callback", stage.port)],
            _ => return Err(invalid()),
        };
        let redirect = if profile == PORTAL_PROFILE {
            format!("http://localhost:{}", stage.port)
        } else {
            expected[0].clone()
        };
        if stage.callback_urls.len() != expected.len()
            || stage
                .callback_urls
                .iter()
                .any(|url| !expected.contains(url))
            || expected.iter().any(|url| {
                stage
                    .callback_urls
                    .iter()
                    .filter(|value| *value == url)
                    .count()
                    != 1
            })
            || stage.callback_url != expected[0]
            || stage.redirect_uri != redirect
        {
            return Err(invalid());
        }
        Ok(stage)
    }
    pub fn value(&self) -> Value {
        json!({ "leaseId":self.lease_id, "callbackUrl":self.callback_url, "callbackUrls":self.callback_urls,
            "redirectUri":self.redirect_uri, "port":self.port, "oauthState":self.oauth_state, "expiresAt":self.expires_at })
    }
    pub fn from_saved(value: &Value, profile: &str, context: &managed::Context) -> Result<Self> {
        Self::parse(value.clone(), profile, context)
    }
}

pub fn open(profile: &str, context: &managed::Context) -> Result<Stage> {
    let value = sdk::host(
        "login.loopback.open",
        json!({"profile":profile,"expected":context.interaction}),
    )?;
    Stage::parse(value, profile, context)
}
pub fn advance(previous: &Stage, context: &managed::Context) -> Result<Stage> {
    let value = sdk::host(
        "login.loopback.kiro.advance",
        json!({"leaseId":previous.lease_id,"expected":context.interaction}),
    )?;
    let stage = Stage::parse(value, AWS_PROFILE, context)?;
    if stage.port != previous.port
        || stage.lease_id == previous.lease_id
        || stage.oauth_state == previous.oauth_state
        || stage.expires_at != previous.expires_at
    {
        return Err(invalid());
    }
    Ok(stage)
}

fn authorization(context: &managed::Context, stage: &Stage, raw: &str) -> Result<()> {
    if raw.is_empty()
        || raw.len() > 8192
        || raw.chars().any(|ch| ch.is_whitespace() || ch.is_control())
        || raw
            .bytes()
            .any(|byte| byte <= b' ' || byte == 127 || byte == b'\\')
    {
        return Err(invalid());
    }
    let parsed = url::Url::parse(raw).map_err(|_| invalid())?;
    let origin = parsed.origin().ascii_serialization();
    if parsed.scheme() != "https"
        || !context.authorization_origins.contains(&origin)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(invalid());
    }
    if raw.split('?').next() != Some(format!("{origin}{}", parsed.path()).as_str()) {
        return Err(invalid());
    }
    let mut query = HashMap::new();
    for (key, value) in parsed.query_pairs() {
        if value.chars().any(char::is_control)
            || query.insert(key.into_owned(), value.into_owned()).is_some()
        {
            return Err(invalid());
        }
    }
    let required: &[&str] = if stage.redirect_uri == format!("http://localhost:{}", stage.port) {
        if origin != endpoints::PORTAL
            || parsed.path() != "/signin"
            || query.get("redirect_from").map(String::as_str) != Some("KiroIDE")
        {
            return Err(invalid());
        }
        &[
            "state",
            "code_challenge",
            "code_challenge_method",
            "redirect_uri",
            "redirect_from",
        ]
    } else {
        if !sdk::aws_oidc::ORIGINS.contains(&origin.as_str())
            || parsed.path() != "/authorize"
            || query.get("response_type").map(String::as_str) != Some("code")
            || query.get("client_id").is_none_or(|id| {
                id.is_empty() || id.len() > 4096 || id.chars().any(char::is_whitespace)
            })
            || query.get("scopes").map(String::as_str) != Some(endpoints::SCOPES.join(",").as_str())
        {
            return Err(invalid());
        }
        &[
            "response_type",
            "client_id",
            "redirect_uri",
            "scopes",
            "state",
            "code_challenge",
            "code_challenge_method",
        ]
    };
    if query.len() != required.len()
        || query.keys().any(|key| !required.contains(&key.as_str()))
        || query.get("state").map(String::as_str) != Some(stage.oauth_state.as_str())
        || query.get("redirect_uri").map(String::as_str) != Some(stage.redirect_uri.as_str())
        || query.get("code_challenge_method").map(String::as_str) != Some("S256")
        || query.get("code_challenge").is_none_or(|value| {
            value.len() != 43
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        })
    {
        return Err(invalid());
    }
    Ok(())
}

pub fn publish(
    context: &managed::Context,
    stage: &Stage,
    title: &str,
    message: &str,
    url: &str,
) -> Result<String> {
    authorization(context, stage, url)?;
    let step =
        managed::publish_browser(&context.interaction, title, message, url, &stage.lease_id)?;
    let id = text(&step, "id", 256)?;
    if !nonce(&id)
        || id == context.interaction
        || step["type"] != "managed"
        || step["provider"] != context.provider
        || step["flow"] != context.flow
        || step["methodIndex"] != 0
        || step["status"] != "ready"
        || step["expiresAt"].as_i64() != Some(context.expires_at)
        || !step["captcha"].is_null()
        || step["browser"]["url"] != url
        || !matches!(step["browser"]["browserMode"].as_str(), Some("external" | "embedded"))
        || step["browser"]["manualCallback"] != true
        || step["browser"]["callbackInput"] != "url"
    {
        return Err(invalid());
    }
    Ok(id)
}
