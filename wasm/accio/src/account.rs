use agent2api_wasm_sdk::{json, Error, Result, Value};

use crate::{credentials::Credentials, endpoints, errors, network, util};

pub fn identity(secret: &Value) -> Result<String> {
    let credential = Credentials::parse(secret)?;
    if !credential.user.is_empty()
        && credential
            .secrets()
            .iter()
            .filter(|secret| !secret.is_empty())
            .any(|secret| credential.user.contains(*secret))
    {
        return Err(Error::http(400, "Accio 账号标识不能包含凭据本体"));
    }
    Ok(format!(
        "accio-intl-{}",
        if credential.user.is_empty() {
            util::fingerprint(&credential.access)
        } else {
            credential.user
        }
    ))
}

pub fn view(secret: &Value) -> Result<Value> {
    let credential = Credentials::parse(secret)?;
    let tail: String = if credential.access.chars().count() <= 8 {
        String::new()
    } else {
        credential
            .access
            .chars()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    };
    let tail = if errors::redact(&tail, &credential.secrets()) == tail {
        tail
    } else {
        String::new()
    };
    let mut value = json!({ "name": util::text(secret, &["name"]), "userId": credential.user,
        "email": credential.email, "nickname": credential.name, "source": util::text(secret, &["source"]),
        "tokenTail": tail, "edition": endpoints::EDITION, "editionLabel": "国际版",
        "hasRefreshToken": !credential.refresh.is_empty(), "canRefresh": !credential.refresh.is_empty(),
        "desktop": false, "available": true });
    if let Some(expires) = credential.expires {
        value["expiresAt"] = json!(expires);
    }
    let mut value = errors::sanitize(&value, &credential.secrets());
    value["tokenTail"] = json!(tail);
    Ok(value)
}

pub fn normalized(credential: &Credentials, source: &str, name: Option<&str>) -> Result<Value> {
    let mut secret = credential.value();
    secret["source"] = json!(source);
    let explicit = name.map(str::trim).filter(|name| !name.is_empty());
    let name = explicit.map(str::to_string).unwrap_or_else(|| {
        if !credential.name.is_empty() {
            credential.name.clone()
        } else if !credential.email.is_empty() {
            credential.email.clone()
        } else if !credential.user.is_empty() {
            format!("Accio {}", credential.user)
        } else {
            "Accio 国际版账号".into()
        }
    });
    secret["name"] = json!(errors::redact(&name, &credential.secrets())
        .chars()
        .filter(|character| !character.is_control())
        .take(100)
        .collect::<String>());
    Ok(
        json!({ "identity": identity(&secret)?, "secret": secret, "public": view(&secret)?, "desktop": false }),
    )
}

fn json_input(input: &str) -> Result<Value> {
    if input.len() > 128 * 1024 {
        return Err(Error::http(400, "Accio 凭据 JSON 过大"));
    }
    let parsed: Value =
        serde_json::from_str(input).map_err(|_| Error::http(400, "Accio 凭据 JSON 格式无效"))?;
    let source = parsed
        .get("pluginSecret")
        .or_else(|| parsed.get("secret"))
        .filter(|value| value.is_object())
        .unwrap_or(&parsed);
    if !source.is_object() {
        return Err(Error::http(400, "Accio 凭据 JSON 必须为对象"));
    }
    if let Some(provider) = parsed.get("provider").and_then(Value::as_str) {
        if provider != endpoints::ID {
            return Err(Error::http(400, "凭据 JSON 不属于 Accio 国际版"));
        }
    }
    crate::credentials::international(&parsed)?;
    let mut source = source.clone();
    for keys in [
        &["accessToken", "access_token", "token"][..],
        &["refreshToken", "refresh_token"][..],
        &["expiresAt", "expires_at", "expires"][..],
        &["userId", "user_id", "uid"][..],
        &["deviceId", "device_id", "utdid"][..],
        &["sgcookie", "sgCookie"][..],
    ] {
        if keys.iter().any(|key| {
            source.get(*key).is_some_and(|value| {
                !value.is_null() && !value.as_str().is_some_and(|text| text.trim().is_empty())
            })
        }) {
            continue;
        }
        if let Some((key, value)) = keys.iter().find_map(|key| {
            parsed
                .get(*key)
                .filter(|value| {
                    !value.is_null() && !value.as_str().is_some_and(|text| text.trim().is_empty())
                })
                .map(|value| (*key, value))
        }) {
            source[key] = value.clone();
        }
    }
    Ok(source)
}

fn input(params: &Value) -> Result<Value> {
    let source = &params["source"];
    match source["kind"].as_str() {
        Some("manual") => {
            let payload = &source["payload"];
            if let Some(index) = payload.get("methodIndex") {
                let index = index
                    .as_u64()
                    .filter(|index| matches!(*index, 1 | 2))
                    .ok_or_else(|| Error::http(400, "Accio 登录方式无效"))?;
                let fields = payload["inputs"]
                    .as_object()
                    .ok_or_else(|| Error::http(400, "Accio 登录字段必须为对象"))?;
                let allowed: &[&str] = if index == 1 {
                    &["accessToken", "refreshToken", "name"]
                } else {
                    &["credentialJson", "name"]
                };
                if fields
                    .iter()
                    .any(|(key, value)| !allowed.contains(&key.as_str()) || !value.is_string())
                {
                    return Err(Error::http(400, "Accio 登录包含未申报字段"));
                }
                let mut value = if index == 2 {
                    json_input(payload["inputs"]["credentialJson"].as_str().unwrap_or(""))?
                } else {
                    Value::Object(fields.clone())
                };
                if let Some(name) = fields.get("name") {
                    value["name"] = name.clone();
                }
                Ok(value)
            } else if payload.is_object() {
                Ok(payload.clone())
            } else if let Some(input) = payload.as_str() {
                json_input(input)
            } else {
                Err(Error::http(400, "Accio 账号内容必须为凭据对象"))
            }
        }
        Some("file_text") => json_input(source["content"].as_str().unwrap_or("")),
        Some("desktop") => Err(Error::http(
            400,
            "Accio 登录态位于客户端加密存储，没有已授权的本机导入能力，请用网页登录或粘贴凭证",
        )),
        _ => Err(Error::http(
            400,
            "Accio 请使用网页登录、手工令牌或凭据 JSON",
        )),
    }
}

pub fn normalize(params: &Value) -> Result<Value> {
    let mut payload = input(params)?;
    crate::credentials::international(&payload)?;
    // 兼容旧 token 表单中直接粘贴的凭据 JSON，不把整段 JSON 当令牌出网。
    if let Some(raw) = payload
        .get("accessToken")
        .or_else(|| payload.get("access_token"))
        .or_else(|| payload.get("token"))
        .and_then(Value::as_str)
        .filter(|text| text.trim_start().starts_with('{'))
    {
        let mut parsed = json_input(raw)?;
        for key in [
            "refreshToken",
            "refresh_token",
            "name",
            "deviceId",
            "sgcookie",
        ] {
            if let Some(value) = payload.get(key).filter(|value| {
                !value.is_null() && !value.as_str().is_some_and(|value| value.trim().is_empty())
            }) {
                parsed[key] = value.clone();
            }
        }
        payload = parsed;
    }
    let mut credential = Credentials::parse(&payload)?;
    // 有手填身份也只读核对一次，不能把另一账号的 token 保存到自报 userId 下。
    match network::profile(&credential) {
        Ok(profile) => {
            let declared = credential.user.clone();
            credential.profile(&profile);
            if !declared.is_empty() && !credential.user.is_empty() && declared != credential.user {
                return Err(Error::http(400, "Accio 凭据与提供的账号身份不一致，未保存"));
            }
        }
        Err(error) if errors::host_stop(&error) => return Err(error),
        // 保留原有手工有效凭据的离线导入，资料不可用不虚构身份或删除原功能。
        Err(_) => {}
    }
    credential.ensure_device(true)?;
    let name = params["name"].as_str().or_else(|| payload["name"].as_str());
    normalized(&credential, "manual", name)
}
