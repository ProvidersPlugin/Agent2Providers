//! 只读当前账号，模型Bearer与账号Cookie分离。身份逐字沿用原生uid/accountId/8字节摘要。
//! 上传astron-session.json与已保存secret可归一；本机导入必须显式调用受控helper。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
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

pub fn scalar(value: &Value) -> String {
    value
        .as_str()
        .map(str::trim)
        .map(str::to_string)
        .or_else(|| value.as_i64().map(|value| value.to_string()))
        .or_else(|| value.as_u64().map(|value| value.to_string()))
        .unwrap_or_default()
}

pub fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
        .filter(|value| value.is_finite())
}

pub fn safe_text(value: &str, max: usize) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .take(max)
        .collect()
}

fn credential(value: &str, cookie: bool) -> Result<String> {
    let value = value.trim();
    if value.len() > 8192
        || !value.is_ascii()
        || value.bytes().any(|byte| {
            byte.is_ascii_control()
                || byte.is_ascii_whitespace()
                || cookie && matches!(byte, b';' | b',')
        })
    {
        return Err(Error::http(400, "AStudio 凭据格式异常或过长"));
    }
    Ok(value.to_string())
}

fn bearer(value: &str) -> &str {
    let value = value.trim();
    match value.get(..7) {
        Some(prefix) if prefix.eq_ignore_ascii_case("bearer ") => value[7..].trim(),
        _ => value,
    }
}

pub fn model_key(secret: &Value) -> Result<String> {
    let key = credential(
        bearer(&text(secret, &["key", "modelBearerToken", "apiKey"])),
        false,
    )?;
    if key.is_empty() {
        return Err(super::errors::expired("model"));
    }
    Ok(key)
}

/// 目录票据与模型网关票据可能分别保存在官方会话和配置中；旧单票据账号仍兼容。
pub fn catalog_key(secret: &Value) -> Result<String> {
    let key = text(secret, &["modelBearerToken"]);
    if key.is_empty() {
        model_key(secret)
    } else {
        credential(bearer(&key), false)
    }
}

pub fn ensure_current(record: &Value) -> Result<()> {
    let current = sdk::account()?;
    if sdk::account_disabled(&current)
        || ["id", "provider", "identity", "desktop", "secret"]
            .iter()
            .any(|key| current[*key] != record[*key])
    {
        return Err(Error::http(409, "AStudio 账号在操作期间已变化，请重新发起"));
    }
    Ok(())
}

fn session_value(secret: &Value, key: &str) -> String {
    let aliases: &[&str] = match key {
        "accountId" => &["accountId", "account_id"],
        "ssoSessionId" => &["ssoSessionId", "sso_sessionid"],
        _ => &[key],
    };
    let nested = if key == "accountId" {
        aliases
            .iter()
            .map(|key| scalar(&secret["session"][*key]))
            .find(|value| !value.is_empty())
            .unwrap_or_default()
    } else {
        text(&secret["session"], aliases)
    };
    if !nested.is_empty() {
        return nested;
    }
    match key {
        "token" => text(secret, &["sessionToken", "token"]),
        "accountId" => aliases
            .iter()
            .map(|key| scalar(&secret[*key]))
            .find(|value| !value.is_empty())
            .unwrap_or_default(),
        "ssoSessionId" => text(secret, aliases),
        _ => String::new(),
    }
}

pub fn has_session(secret: &Value) -> bool {
    (!session_value(secret, "token").is_empty() && !session_value(secret, "accountId").is_empty())
        || !session_value(secret, "ssoSessionId").is_empty()
}

pub fn cookie(secret: &Value) -> Result<String> {
    let token = credential(&session_value(secret, "token"), true)?;
    let account = credential(&session_value(secret, "accountId"), true)?;
    let sso = credential(&session_value(secret, "ssoSessionId"), true)?;
    if !has_session(secret) {
        return Err(
            Error::http(400, "此 AStudio 账号只有模型凭据；积分与签到需要网页会话")
                .with_code("astudio_no_session"),
        );
    }
    let mut parts = Vec::new();
    if !sso.is_empty() {
        parts.push(format!("ssoSessionId={sso}"));
        parts.push(format!("sso_sessionid={sso}"));
    }
    if !account.is_empty() {
        parts.push(format!("account_id={account}"));
    }
    if !token.is_empty() {
        parts.push(format!("token={token}"));
    }
    Ok(parts.join("; "))
}

pub fn identity(secret: &Value) -> String {
    let uid = scalar(&secret["uid"]);
    if !uid.is_empty() {
        return format!("astudio:{uid}");
    }
    let account = session_value(secret, "accountId");
    if !account.is_empty() {
        return format!("astudio:{account}");
    }
    let key = text(secret, &["key", "modelBearerToken", "apiKey"]);
    let hex: String = Sha256::digest(key.as_bytes())
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("astudio:key:{hex}")
}

fn has_credentials(secret: &Value) -> bool {
    !text(secret, &["key", "modelBearerToken", "apiKey"]).is_empty()
}

/// 导入和登录中的占位 uid=0 不是用户身份，必须补 userInfo 或回退 accountId。
pub fn login_uid(value: &Value) -> String {
    let value = scalar(value);
    if value == "0" {
        String::new()
    } else {
        value
    }
}

fn public_identifier(value: &str, secret: &Value) -> String {
    let protected = [
        text(secret, &["key", "apiKey"]),
        text(secret, &["modelBearerToken"]),
        session_value(secret, "token"),
        session_value(secret, "ssoSessionId"),
    ];
    if protected
        .iter()
        .any(|key| !key.is_empty() && value.contains(key))
    {
        String::new()
    } else {
        safe_text(value, 480)
    }
}

pub fn view(secret: &Value) -> Value {
    let uid = public_identifier(&scalar(&secret["uid"]), secret);
    let account = public_identifier(&session_value(secret, "accountId"), secret);
    let mut name = public_identifier(&text(secret, &["name"]), secret);
    if name.is_empty() {
        name = uid.clone();
    }
    if name.is_empty() {
        name = account.clone();
    }
    if name.is_empty() {
        name = "AStudio".into();
    }
    let key = text(secret, &["key", "modelBearerToken", "apiKey"]);
    let tail: String = key
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let mut public = json!({ "name": safe_text(&name, 100), "uid": uid, "accountId": account,
        "tokenTail": tail, "hasCredentials": !key.is_empty(), "hasAccessToken": !key.is_empty(),
        "hasSession": has_session(secret), "hasRefreshToken": false, "canRefresh": false });
    if secret["source"] == "desktop" {
        public["desktop"] = json!(true);
    }
    if !has_session(secret) {
        public["noSession"] = json!(true);
    }
    public
}

pub fn parse(payload: &Value, source: &str) -> Result<Value> {
    let mut input = payload;
    for key in ["pluginSecret", "secret"] {
        if input[key].is_object() {
            input = &input[key];
        }
    }
    if input["banned"] == true {
        return Err(Error::http(403, "AStudio 账号已被封禁"));
    }
    let key = bearer(&text(input, &["key", "modelBearerToken", "apiKey"])).to_string();
    let key = if key.is_empty() && !has_session(input) {
        bearer(&text(input, &["token"])).to_string()
    } else {
        key
    };
    let uid = login_uid(&input["uid"]);
    let account = credential(&session_value(input, "accountId"), true)?;
    if uid.len() > 480 || account.len() > 480 || uid.chars().any(char::is_control) {
        return Err(Error::http(400, "AStudio 账号标识格式不正确"));
    }
    let token = credential(&session_value(input, "token"), true)?;
    let sso = credential(&session_value(input, "ssoSessionId"), true)?;
    let key = credential(&key, false)?;
    let mut secret = json!({ "key": key, "uid": uid, "source": source });
    let catalog_key = credential(bearer(&text(input, &["modelBearerToken"])), false)?;
    if !catalog_key.is_empty() && catalog_key != key {
        secret["modelBearerToken"] = json!(catalog_key);
    }
    let name = text(input, &["name"]);
    if !name.is_empty() {
        secret["name"] = json!(safe_text(&name, 100));
    }
    if !sso.is_empty() || !token.is_empty() && !account.is_empty() {
        let mut session = json!({});
        if !token.is_empty() {
            session["token"] = json!(token);
        }
        if !account.is_empty() {
            session["accountId"] = json!(account);
        }
        if !sso.is_empty() {
            session["ssoSessionId"] = json!(sso);
        }
        secret["session"] = session;
    } else if !token.is_empty() || !account.is_empty() || input["session"].is_object() {
        // 旧 token-only 导入仍是模型凭据，不能把它假装成账号 Cookie。
        if !account.is_empty()
            || !text(input, &["sessionToken"]).is_empty()
            || input["session"].is_object()
        {
            return Err(Error::http(
                400,
                "AStudio 网页会话需要 token 与 accountId，或有效 ssoSessionId",
            ));
        }
    }
    if !has_credentials(&secret) && !has_session(&secret) {
        return Err(Error::http(
            400,
            "请填写 AStudio modelBearerToken 或有效网页会话",
        ));
    }
    Ok(secret)
}

pub fn normalized(mut secret: Value, name: Option<&str>) -> Result<Value> {
    if !has_credentials(&secret) {
        super::auth::resolve_model(&mut secret, || Ok(()))?;
    }
    if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
        secret["name"] = json!(safe_text(name, 100));
    }
    model_key(&secret)?;
    let id = identity(&secret);
    if id.len() > 512 || id.chars().any(char::is_control) {
        return Err(Error::http(400, "AStudio 账号身份格式异常"));
    }
    Ok(
        json!({ "identity": id, "public": view(&secret), "desktop": secret["source"] == "desktop", "secret": secret }),
    )
}

fn parse_text(content: &str) -> Result<Value> {
    let content = content.trim();
    if content.is_empty() || content.len() > 256 * 1024 {
        return Err(Error::http(400, "AStudio 凭据文本为空或超过大小限制"));
    }
    if content.starts_with('{') {
        serde_json::from_str(content).map_err(|_| Error::http(400, "AStudio 凭据不是有效 JSON"))
    } else {
        Ok(json!({ "modelBearerToken": content }))
    }
}

pub fn normalize(params: &Value) -> Result<Value> {
    let source = &params["source"];
    if source["kind"] == "desktop" {
        return normalized(super::desktop::read()?, params["name"].as_str());
    }
    let mut payload = match source["kind"].as_str() {
        Some("manual") => {
            let input = &source["payload"];
            if input["inputs"].is_object() {
                input["inputs"].clone()
            } else {
                input.clone()
            }
        }
        Some("file_text") => parse_text(source["content"].as_str().unwrap_or(""))?,
        _ => {
            return Err(Error::http(
                400,
                "AStudio 请使用短信、密码、凭据文本或显式本机导入",
            ))
        }
    };
    if let Some(content) = payload.as_str() {
        payload = parse_text(content)?;
    }
    let embedded = text(&payload, &["credentialJson"]);
    if !embedded.is_empty() {
        payload = parse_text(&embedded)?;
    }
    let token = text(&payload, &["modelBearerToken"]);
    if token.trim_start().starts_with('{') {
        payload = parse_text(&token)?;
    }
    normalized(parse(&payload, "paste")?, params["name"].as_str())
}

pub fn current() -> Result<Value> {
    let record = sdk::account()?;
    if sdk::account_disabled(&record) {
        return Err(Error::http(403, "AStudio 账号已停用").with_code("local_precondition"));
    }
    model_key(&record["secret"])?;
    Ok(record)
}
