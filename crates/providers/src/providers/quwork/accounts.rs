use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use base64::Engine as _;
use serde_json::{json, Value};

use super::{client, desktop, endpoints};
use crate::errors::GatewayError;

pub const MAX_CREDENTIAL_BYTES: usize = 262_144;

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| {
            value
                .get(*key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
        })
        .unwrap_or("")
        .to_string()
}

pub fn user_view(user: &Value) -> Value {
    let mut public = json!({});
    for (target, keys) in [
        ("uid", &["id", "uid", "userId"][..]),
        ("username", &["username"][..]),
        ("nickname", &["display_name", "displayName", "nickname"][..]),
        ("phone", &["phone"][..]),
    ] {
        if let Some(value) = keys.iter().find_map(|key| {
            user.get(*key)
                .filter(|value| value.is_string() || value.is_number())
        }) {
            public[target] = if value.is_string() {
                value.clone()
            } else {
                json!(value.to_string())
            };
        }
    }
    public
}

pub fn token(value: &str, label: &str) -> Result<String, GatewayError> {
    let value = value.trim();
    let value = value.strip_prefix("Bearer ").unwrap_or(value).trim();
    if value.len() > 16_384
        || !value.is_ascii()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(GatewayError::with_status(
            400,
            format!("QuWork {label}格式不正确"),
        ));
    }
    Ok(value.to_string())
}

pub fn parse(value: &Value) -> Result<Value, GatewayError> {
    parse_inner(value, 0)
}

fn parse_inner(value: &Value, depth: usize) -> Result<Value, GatewayError> {
    if depth > 6 {
        return Err(GatewayError::with_status(400, "QuWork 凭证嵌套过深"));
    }
    let mut input = value.clone();
    for key in ["secret", "session", "data"] {
        if let Some(nested) = input.get(key).filter(|value| value.is_object()) {
            input = nested.clone();
        }
    }
    let nested_llm = input
        .get("llmToken")
        .and_then(Value::as_str)
        .filter(|text| text.trim().starts_with('{'));
    let mut parsed = if let Some(content) = input
        .get("credentials")
        .and_then(Value::as_str)
        .or(nested_llm)
    {
        parse_text_inner(content, depth + 1)?
    } else {
        json!({})
    };
    let access = token(
        &text(&input, &["accessToken", "access_token", "token"]),
        "登录凭证",
    )?;
    let llm = if nested_llm.is_some() {
        String::new()
    } else {
        token(
            &text(&input, &["llmToken", "llm_token", "apiKey", "key"]),
            "模型凭证",
        )?
    };
    if !access.is_empty() {
        parsed["accessToken"] = json!(access);
    }
    if !llm.is_empty() && !llm.starts_with('{') {
        parsed["llmToken"] = json!(llm);
    }
    if let Some(user) = input.get("user").filter(|value| value.is_object()) {
        parsed["user"] = user_view(user);
    } else if input.get("uid").is_some() {
        parsed["user"] = user_view(&input);
    }
    let expires = client::timestamp(input.get("expiresAt"))
        .or_else(|| client::timestamp(input.get("expire")))
        .or_else(|| jwt_expiry(&text(&parsed, &["accessToken"])));
    if let Some(expires) = expires {
        parsed["expiresAt"] = json!(expires);
    }
    for key in ["name", "deviceIdentifier", "deviceUuid"] {
        let value = text(&input, &[key]);
        if !value.is_empty() {
            if value.len() > 256
                || value.chars().any(char::is_control)
                || (key == "deviceIdentifier"
                    && (value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit())))
            {
                return Err(GatewayError::with_status(400, "QuWork 账号字段格式不正确"));
            }
            parsed[key] = json!(value);
        }
    }
    if text(&parsed, &["llmToken"]).is_empty() {
        return Err(GatewayError::with_status(
            400,
            "缺少 QuWork 模型凭证 llmToken；登录凭证不能代替模型凭证，请先在 QuWork 客户端完成登录",
        ));
    }
    Ok(parsed)
}

pub fn parse_text(content: &str) -> Result<Value, GatewayError> {
    parse_text_inner(content, 0)
}

fn parse_text_inner(content: &str, depth: usize) -> Result<Value, GatewayError> {
    if depth > 6 {
        return Err(GatewayError::with_status(400, "QuWork 凭证嵌套过深"));
    }
    if content.len() > MAX_CREDENTIAL_BYTES {
        return Err(GatewayError::with_status(
            400,
            "QuWork 凭证内容超过大小限制",
        ));
    }
    let content = content.trim().trim_start_matches('\u{feff}').trim();
    if content.starts_with('{') {
        let value: Value = serde_json::from_str(content)
            .map_err(|_| GatewayError::with_status(400, "QuWork 凭证 JSON 格式不正确"))?;
        return parse_inner(&value, depth + 1);
    }
    if content
        .lines()
        .any(|line| line.trim_start().starts_with("QURIVS_LLM_API_KEY:"))
    {
        let value = content
            .lines()
            .find_map(|line| line.trim().strip_prefix("QURIVS_LLM_API_KEY:"))
            .unwrap_or("")
            .trim();
        let value = if value.starts_with('"') {
            serde_json::from_str::<String>(value)
                .map_err(|_| GatewayError::with_status(400, "QuWork 模型凭证格式不正确"))?
        } else {
            value.trim_matches('\'').to_string()
        };
        return parse_inner(&json!({ "llmToken": value }), depth + 1);
    }
    parse_inner(&json!({ "llmToken": content }), depth + 1)
}

pub fn jwt_expiry(token: &str) -> Option<i64> {
    let part = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(part.trim_end_matches('='))
        .ok()?;
    let body: Value = serde_json::from_slice(&bytes).ok()?;
    client::timestamp(body.get("exp"))
}

pub fn identity(secret: &Value) -> String {
    let user = secret.get("user").unwrap_or(&Value::Null);
    let uid = text(user, &["uid", "id"]);
    if !uid.is_empty() {
        return format!("quwork:{uid}");
    }
    let key = text(secret, &["llmToken", "modelFingerprint"]);
    if key.is_empty() {
        return String::new();
    }
    if secret.get("modelFingerprint").is_some() && secret.get("llmToken").is_none() {
        format!("quwork:key:{key}")
    } else {
        format!("quwork:key:{}", crate::flight::fingerprint(&key))
    }
}

pub fn public(secret: &Value) -> Value {
    let user = secret.get("user").unwrap_or(&Value::Null);
    let nickname = text(
        user,
        &[
            "nickname",
            "display_name",
            "displayName",
            "username",
            "phone",
            "uid",
        ],
    );
    let name = text(secret, &["name"]);
    let llm = text(secret, &["llmToken"]);
    let local = secret.get("source").and_then(Value::as_str) == Some("desktop");
    let has_login = !text(secret, &["accessToken"]).is_empty()
        || secret.get("hasLoginToken").and_then(Value::as_bool) == Some(true);
    let mut view = json!({
        "name": if name.is_empty() { if nickname.is_empty() { "QuWork" } else { &nickname } } else { &name },
        "uid": text(user, &["uid", "id"]),
        "nickname": nickname,
        "phone": text(user, &["phone"]),
        "hasToken": !llm.is_empty() || local,
        "available": !llm.is_empty() || local,
        "hasLoginToken": has_login,
        "hasAccessToken": has_login,
        "canRefresh": has_login,
        "tokenTail": if llm.is_empty() { text(secret, &["tokenTail"]) } else { llm.chars().rev().take(6).collect::<String>().chars().rev().collect() },
        "desktopAuth": local,
    });
    for key in ["expiresAt", "updatedAt"] {
        if let Some(value) = secret.get(key).filter(|value| value.is_number()) {
            view[key] = value.clone();
        }
    }
    view
}

pub fn normalized(mut secret: Value, name: Option<&str>, local: bool) -> proto::NormalizedAccount {
    if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
        secret["name"] = json!(name.chars().take(128).collect::<String>());
    }
    proto::NormalizedAccount {
        identity: identity(&secret),
        public: public(&secret),
        secret,
        desktop: local,
    }
}

pub async fn normalize(
    ctx: &PluginCtx,
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, String> {
    let local = matches!(input.source, proto::NormalizeSource::Desktop);
    let result = async {
        let mut secret = match input.source {
            proto::NormalizeSource::Desktop => desktop::reference().await?,
            proto::NormalizeSource::Manual { payload } => {
                let fields = payload
                    .get("inputs")
                    .filter(|value| value.is_object())
                    .unwrap_or(&payload);
                parse(fields)?
            }
            proto::NormalizeSource::FileText { content, .. } => parse_text(&content)?,
            proto::NormalizeSource::FileBase64 { .. } => {
                return Err(GatewayError::with_status(
                    400,
                    "QuWork 请使用本机导入、凭证 JSON 或 .credentials.yaml 文本，不接受二进制文件",
                ))
            }
        };
        if !local {
            let access = text(&secret, &["accessToken"]);
            if !access.is_empty() {
                let reply = client::request(
                    &client::client(ctx, input.proxy.as_ref()),
                    reqwest::Method::GET,
                    endpoints::SELF_PATH,
                    &access,
                    None,
                )
                .await?;
                let user = reply.data("账号查询")?;
                if !user.is_object() {
                    return Err(GatewayError::with_status(502, "QuWork 账号信息格式异常"));
                }
                secret["user"] = user_view(&user);
            }
        }
        Ok(normalized(secret, input.name.as_deref(), local))
    }
    .await;
    result.map_err(GatewayError::into_control_message)
}
