//! 凭据只属于当前账号。identity沿用Cline external_id→展示身份→摘要规则。
//! OAuth保留workos:前缀，公开视图只给尾号和身份。

use super::endpoints;
use agent2api_wasm_sdk::{json, Error, Result, Value};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sha2::{Digest, Sha256};

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| {
            value
                .get(*key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or("")
        .to_string()
}

pub fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
        .filter(|value: &f64| value.is_finite())
}

pub fn expires(value: &Value) -> Option<i64> {
    if let Some(number) = number(value).filter(|number| *number > 0.0 && number.fract() == 0.0) {
        let millis = if number < 100_000_000_000.0 {
            number * 1000.0
        } else {
            number
        };
        return (millis < i64::MAX as f64).then_some(millis as i64);
    }
    chrono::DateTime::parse_from_rfc3339(value.as_str()?.trim())
        .ok()
        .map(|value| value.timestamp_millis())
        .filter(|value| *value > 0)
}

pub fn clean_token(value: &str) -> Result<String> {
    let mut value = value.trim();
    // Header names and the Bearer scheme are case-insensitive; tolerate copied header values.
    while value
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Bearer "))
    {
        value = value[7..].trim();
    }
    if value.len() > endpoints::MAX_TOKEN
        || !value.is_ascii()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(Error::http(400, "Cline 凭证格式不正确或过长"));
    }
    Ok(value.to_string())
}

pub fn access(secret: &Value) -> Result<String> {
    let token = clean_token(&text(secret, &["accessToken", "access_token", "token"]))?;
    let raw = token.strip_prefix(endpoints::PREFIX).unwrap_or(&token);
    if raw.is_empty() {
        return Err(Error::http(401, "Cline 账号缺少访问令牌，请重新登录").with_code("cline_token"));
    }
    if raw.starts_with(endpoints::PREFIX)
        || raw.len() + endpoints::PREFIX.len() > endpoints::MAX_TOKEN
    {
        return Err(Error::http(400, "Cline 凭证格式不正确或过长"));
    }
    Ok(format!("{}{raw}", endpoints::PREFIX))
}

pub fn claims(token: &str) -> Value {
    let token = token.trim().trim_start_matches(endpoints::PREFIX);
    token
        .split('.')
        .nth(1)
        .and_then(|part| URL_SAFE_NO_PAD.decode(part.trim_end_matches('=')).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}

pub fn jwt_expiry(token: &str) -> Option<i64> {
    number(&claims(token)["exp"])
        .filter(|seconds| {
            *seconds > 0.0 && seconds.fract() == 0.0 && *seconds < (i64::MAX / 1000) as f64
        })
        .and_then(|seconds| (seconds as i64).checked_mul(1000))
}

pub fn expiry(secret: &Value) -> Option<i64> {
    let declared = expires(&secret["expiresAt"]);
    let jwt = access(secret).ok().and_then(|token| jwt_expiry(&token));
    match (declared, jwt) {
        (Some(declared), Some(jwt)) => Some(declared.min(jwt)),
        (declared, jwt) => declared.or(jwt),
    }
}

pub fn user_id(token: &str) -> String {
    text(&claims(token), &["external_id"])
}

fn cjk(value: &str) -> bool {
    value.chars().any(|ch| matches!(ch as u32, 0x3040..=0x30ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff | 0xac00..=0xd7af))
}

fn display(info: &Value, allow_name: bool) -> String {
    let email = text(info, &["email"]);
    if !email.is_empty() {
        return email;
    }
    let first = text(info, &["firstName"]);
    let last = text(info, &["lastName"]);
    if !first.is_empty() || !last.is_empty() {
        return if first.is_empty() {
            last
        } else if last.is_empty() {
            first
        } else if cjk(&first) || cjk(&last) {
            format!("{last}{first}")
        } else {
            format!("{first} {last}")
        };
    }
    if allow_name {
        let name = text(info, &["name"]);
        if !name.is_empty() {
            return name;
        }
    }
    // 旧JWT身份兜底不读name；仅桌面userInfo允许它，不能借迁移改变去重身份。
    text(info, &["external_id", "clineUserId", "sub"])
}

pub fn fingerprint(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn identity(secret: &Value) -> String {
    let account = text(secret, &["account", "userId", "accountId"]);
    if !account.is_empty() {
        return account;
    }
    let raw = text(secret, &["accessToken", "access_token", "token"]);
    let Ok(token) = clean_token(&raw) else {
        return String::new();
    };
    if token.is_empty() {
        return String::new();
    }
    let id = user_id(&token);
    if !id.is_empty() {
        return id;
    }
    let name = display(&claims(&token), false);
    if !name.is_empty() {
        return name;
    }
    // 无稳定身份的历史记录按原始已保存令牌算摘要，不因补workos:前缀换identity。
    format!("token-{}", fingerprint(&token))
}

pub fn local(secret: &Value) -> bool {
    secret["desktop"] == true || secret["source"] == "desktop"
}

pub fn parse(payload: &Value) -> Result<Value> {
    let mut input = payload;
    for key in ["pluginSecret", "secret", "data", "auth"] {
        if input[key].is_object() {
            input = &input[key];
        }
    }
    if input["providers"].is_object() {
        input = input
            .pointer("/providers/cline/settings/auth")
            .filter(|value| value.is_object())
            .ok_or_else(|| Error::http(400, "providers.json 中没有 Cline 登录凭证"))?;
    }
    if !input.is_object() {
        return Err(Error::http(400, "Cline 凭据必须是单账号 JSON 对象"));
    }
    if let Some(provider) = input.get("provider").and_then(Value::as_str) {
        if provider == "cline-pass" {
            return Err(Error::http(
                400,
                "这份凭据标记为 Cline 订阅池；订阅池已改用内置「Cline Pass」API 模板，请勿导入到免费池",
            ));
        }
        if provider != crate::PROVIDER_ID {
            return Err(Error::http(
                400,
                "这份 Cline 凭据声明了其他提供商，不能导入到免费池",
            ));
        }
    }
    if let Some(pool) = input.get("pool").and_then(Value::as_str) {
        if ["pass", "cline-pass"]
            .iter()
            .any(|other| pool.eq_ignore_ascii_case(other))
        {
            return Err(Error::http(
                400,
                "这份凭据标记为 Cline 订阅池；订阅池已改用内置「Cline Pass」API 模板，请勿导入到免费池",
            ));
        }
    }
    let access = access(input)?;
    let refresh = clean_token(&text(input, &["refreshToken", "refresh_token"]))?;
    let mut secret = json!({ "accessToken": access });
    if !refresh.is_empty() {
        secret["refreshToken"] = json!(refresh);
    }
    let jwt = claims(&access);
    let mut id = text(input, &["account", "userId", "accountId"]);
    if id.is_empty() {
        id = user_id(&access);
    }
    if id.is_empty() {
        id = text(&input["metadata"]["userInfo"], &["clineUserId", "subject"]);
    }
    if id.is_empty() {
        id = text(
            &input["userInfo"],
            &["clineUserId", "external_id", "subject"],
        );
    }
    if id.is_empty() {
        id = display(&jwt, false);
    }
    if id.is_empty() {
        id = identity(input);
    }
    if id.len() > 512 || id.chars().any(char::is_control) {
        return Err(Error::http(400, "Cline 账号标识格式不正确"));
    }
    secret["account"] = json!(id);
    let mut name = text(input, &["displayName", "name"]);
    if name.is_empty() {
        name = display(&input["metadata"]["userInfo"], true);
    }
    if name.is_empty() {
        name = display(&input["userInfo"], true);
    }
    if name.is_empty() {
        name = display(&jwt, false);
    }
    if name.is_empty() {
        name = id;
    }
    secret["displayName"] = json!(name
        .chars()
        .filter(|ch| !ch.is_control())
        .take(100)
        .collect::<String>());
    if let Some(expires) =
        expiry(&json!({ "accessToken": access, "expiresAt": input["expiresAt"] }))
    {
        secret["expiresAt"] = json!(expires);
    }
    Ok(secret)
}

pub fn view(secret: &Value) -> Value {
    let token = text(secret, &["accessToken", "access_token", "token"]);
    let id = identity(secret);
    let name = text(secret, &["displayName", "name"]);
    let name = if name.is_empty() {
        display(&claims(&token), false)
    } else {
        name
    };
    let name = if name.is_empty() {
        if id.is_empty() {
            "Cline 账号".into()
        } else {
            id.clone()
        }
    } else {
        name
    };
    let name: String = name
        .chars()
        .filter(|ch| !ch.is_control())
        .take(100)
        .collect();
    let has_refresh = !text(secret, &["refreshToken", "refresh_token"]).is_empty()
        || secret["hasRefreshToken"] == true;
    let tail = if token.is_empty() {
        text(secret, &["tokenTail"])
    } else {
        token.clone()
    };
    let refresh = text(secret, &["refreshToken", "refresh_token"]);
    let protected = |value: &str| {
        [
            token.as_str(),
            token.strip_prefix(endpoints::PREFIX).unwrap_or(&token),
            refresh.as_str(),
        ]
        .into_iter()
        .any(|credential| !credential.is_empty() && value.contains(credential))
    };
    let name = if protected(&name) {
        String::new()
    } else {
        name
    };
    let id = if protected(&id) { String::new() } else { id };
    let tail: String = tail
        .chars()
        .rev()
        .take(6)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let tail = if protected(&tail) {
        String::new()
    } else {
        tail
    };
    let mut view = json!({ "name": name, "displayName": name, "uid": id, "account": id,
        "tokenTail": tail,
        "hasAccessToken": access(secret).is_ok() || local(secret), "hasRefreshToken": has_refresh,
        "canRefresh": has_refresh && secret["desktopReadOnly"] != true,
        "canSyncCredentials": local(secret), "desktopAuth": local(secret) });
    if let Some(expiry) = expiry(secret) {
        view["expiresAt"] = json!(expiry);
    }
    view
}

pub fn normalized(mut secret: Value, name: Option<&str>) -> Result<Value> {
    let id = identity(&secret);
    if id.is_empty() || id.len() > 512 || id.chars().any(char::is_control) {
        return Err(Error::http(400, "Cline 账号身份缺失或格式不正确"));
    }
    // 本机稳定身份和备注同样要脱敏；短令牌不能以“尾号”完整公开。
    let access = text(&secret, &["accessToken", "access_token", "token"]);
    let refresh = text(&secret, &["refreshToken", "refresh_token"]);
    let raw_access = access.strip_prefix(endpoints::PREFIX).unwrap_or(&access);
    if [&access[..], raw_access, &refresh[..]]
        .into_iter()
        .any(|token| !token.is_empty() && id.contains(token))
    {
        return Err(Error::http(400, "Cline 账号身份不能使用凭据本体"));
    }
    if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
        secret["displayName"] = json!(name
            .chars()
            .filter(|ch| !ch.is_control())
            .take(100)
            .collect::<String>());
    }
    Ok(
        json!({ "identity": id, "public": view(&secret), "desktop": local(&secret), "secret": secret }),
    )
}

pub fn normalize(params: &Value) -> Result<Value> {
    let source = &params["source"];
    let mut payload = match source["kind"].as_str() {
        Some("manual") => {
            let value = &source["payload"];
            if value["inputs"].is_object() {
                value["inputs"].clone()
            } else {
                value.clone()
            }
        }
        Some("file_text") => {
            let content = source["content"].as_str().unwrap_or("");
            if content.len() > 256 * 1024 {
                return Err(Error::http(400, "Cline 凭证文件超过大小限制"));
            }
            serde_json::from_str(content)
                .map_err(|_| Error::http(400, "Cline 凭证文件不是有效 JSON"))?
        }
        Some("desktop") => {
            return normalized(super::desktop::snapshot()?, params["name"].as_str());
        }
        _ => {
            return Err(Error::http(
                400,
                "Cline 只接受设备授权、令牌或 JSON 文本，不接受二进制文件",
            ))
        }
    };
    if let Some(text) = payload["credentialJson"]
        .as_str()
        .filter(|text| !text.trim().is_empty())
    {
        if text.len() > 256 * 1024 {
            return Err(Error::http(400, "Cline 凭证 JSON 超过大小限制"));
        }
        payload = serde_json::from_str(text)
            .map_err(|_| Error::http(400, "Cline 凭证 JSON 格式不正确"))?;
    }
    if payload.is_string() {
        payload = json!({ "accessToken": payload.as_str().unwrap_or("") });
    }
    let secret = parse(&payload)?;
    normalized(secret, params["name"].as_str())
}
