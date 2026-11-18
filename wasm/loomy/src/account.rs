//! 账号始终绑定宿主原行；本机导入保存独立快照，不消费客户端续期材料或估算会话失效。
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub const HELPER: &str = "desktop.credentials.loomy.v1";

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or("")
        .to_string()
}

pub fn identifier(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| match &value[*key] {
            Value::String(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
            Value::Number(value) if value.is_i64() || value.is_u64() => Some(value.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

fn clean_token(value: &str) -> Result<String> {
    let mut value = value.trim();
    while value
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Bearer "))
    {
        value = value[7..].trim();
    }
    if value.is_empty()
        || value.len() > 8192
        || !value.is_ascii()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(Error::http(
            400,
            "Loomy Session 缺失、格式不正确或超过长度限制",
        ));
    }
    Ok(value.to_string())
}

pub fn token(secret: &Value) -> Result<String> {
    let raw = text(secret, &["session", "accessToken", "token"]);
    if raw.is_empty() {
        return Err(Error::http(401, "Loomy 账号缺少 Session，请重新登录")
            .with_code("loomy_authentication"));
    }
    clean_token(&raw)
}

pub fn current() -> Result<Value> {
    let record = sdk::account()?;
    if sdk::account_disabled(&record) {
        return Err(Error::http(403, "Loomy 账号已停用").with_code("local_precondition"));
    }
    token(&record["secret"])?;
    Ok(record)
}

pub fn ensure_current(record: &Value) -> Result<()> {
    let current = sdk::account()?;
    if sdk::account_disabled(&current)
        || ["id", "provider", "identity", "desktop", "secret"]
            .iter()
            .any(|key| current[*key] != record[*key])
    {
        return Err(Error::http(409, "Loomy 账号在操作期间已变化，请重新发起")
            .with_code("local_precondition"));
    }
    Ok(())
}

pub fn session() -> Result<String> {
    token(&current()?["secret"])
}

pub fn tail(session: &str) -> String {
    // 短凭据不能借“尾号”完整公开。
    if session.chars().count() <= 6 {
        return String::new();
    }
    session
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

pub fn mask_phone(phone: &str) -> String {
    let chars: Vec<char> = phone.chars().collect();
    if chars.len() < 7 {
        return phone.to_string();
    }
    format!(
        "{}****{}",
        chars[..3].iter().collect::<String>(),
        chars[chars.len() - 4..].iter().collect::<String>()
    )
}

pub fn default_name(phone: &str, user_id: &str) -> String {
    if phone.chars().count() >= 11 {
        format!("Loomy {}", mask_phone(phone))
    } else if !user_id.is_empty() {
        format!("Loomy {user_id}")
    } else {
        "Loomy 账号".into()
    }
}

pub fn identity(secret: &Value) -> String {
    let uid = identifier(secret, &["userId", "userid", "user_id"]);
    if !uid.is_empty() {
        return format!("loomy-{uid}");
    }
    let phone = identifier(secret, &["phone"]);
    if !phone.is_empty() {
        return format!("loomy-{phone}");
    }
    // 保持历史缺身份记录原尾号主键，不为新导入创建易变身份。
    let raw = text(secret, &["session", "accessToken", "token"]);
    format!(
        "loomy-{}",
        raw.chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>()
    )
}

pub fn view(secret: &Value) -> Result<Value> {
    let session = token(secret)?;
    let safe = |value: String, maximum: usize| -> String {
        if value.contains(&session) {
            return String::new();
        }
        value
            .chars()
            .filter(|ch| !ch.is_control())
            .take(maximum)
            .collect()
    };
    let uid = safe(identifier(secret, &["userId", "userid", "user_id"]), 256);
    let phone = safe(identifier(secret, &["phone"]), 256);
    let name = safe(text(secret, &["name"]), 100);
    Ok(
        json!({ "name": if name.is_empty() { default_name(&phone, &uid) } else { name },
        "uid": uid, "phone": phone, "tokenTail": tail(&session),
        "hasSession": true, "hasAccessToken": true, "hasRefreshToken": false, "canRefresh": false }),
    )
}

pub fn normalized(session: &str, uid: &str, phone: &str, name: Option<&str>) -> Result<Value> {
    let session = clean_token(session)?;
    let uid = uid.trim();
    let phone = phone.trim();
    if uid.len() > 256
        || phone.len() > 256
        || uid.chars().any(char::is_control)
        || phone.chars().any(char::is_control)
        || uid.contains(&session)
        || phone.contains(&session)
    {
        return Err(Error::http(
            400,
            "Loomy 账号标识格式不正确，不能使用凭据本体",
        ));
    }
    if uid.is_empty() && phone.is_empty() {
        return Err(Error::http(
            400,
            "缺少 userId 或手机号，请填写原账号身份或完整登录态 JSON",
        ));
    }
    let mut secret =
        json!({ "session": session, "userId": uid, "phone": phone, "addedAt": sdk::now() });
    if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
        secret["name"] = json!(name
            .chars()
            .filter(|ch| !ch.is_control())
            .take(100)
            .collect::<String>());
    }
    Ok(
        json!({ "identity": identity(&secret), "public": view(&secret)?, "secret": secret, "desktop": false }),
    )
}

fn parsed(raw: &str) -> Result<Value> {
    if raw.trim().is_empty() || raw.len() > 256 * 1024 {
        return Err(Error::http(400, "Loomy 凭据文本为空或超过大小限制"));
    }
    if raw.trim_start().starts_with('{') {
        serde_json::from_str(raw).map_err(|_| Error::http(400, "Loomy 凭据 JSON 格式不正确"))
    } else {
        Ok(json!({ "session": raw.trim() }))
    }
}

fn unpack(value: &Value) -> Result<Value> {
    if !value.is_object() {
        return Err(Error::http(400, "Loomy 凭据必须是单账号 JSON 对象"));
    }
    let mut result = value.clone();
    for _ in 0..8 {
        let nested = ["pluginSecret", "secret", "auth", "data"]
            .iter()
            .find_map(|key| result[*key].as_object().cloned());
        let Some(mut fields) = nested else {
            return Ok(result);
        };
        // 嵌套任一别名有值时保持整组优先，不让外层旧 accessToken 覆盖新 session。
        for group in [
            &["session", "accessToken", "token"][..],
            &["userId", "userid", "user_id"][..],
            &["phone", "name"][..],
        ] {
            if group.len() == 2 && group[0] == "phone" {
                for key in group {
                    if fields
                        .get(*key)
                        .is_none_or(|value| value.is_null() || value == "")
                    {
                        if let Some(value) = result.get(*key) {
                            fields.insert((*key).into(), value.clone());
                        }
                    }
                }
            } else if !group.iter().any(|key| {
                fields
                    .get(*key)
                    .is_some_and(|value| !value.is_null() && value != "")
            }) {
                for key in group {
                    if let Some(value) = result.get(*key) {
                        fields.insert((*key).into(), value.clone());
                    }
                }
            }
        }
        result = Value::Object(fields);
    }
    Err(Error::http(400, "Loomy 凭据嵌套层数超过限制"))
}

pub fn normalize(params: &Value) -> Result<Value> {
    let source = &params["source"];
    if source["kind"] == "desktop" {
        let response = sdk::native_call(HELPER, "read", json!({}))?;
        let content = response["content"]
            .as_str()
            .ok_or_else(|| Error::http(502, "Loomy 本机读取未返回登录快照"))?;
        let payload = unpack(&parsed(content)?)?;
        let session = token(&payload)?;
        let uid = identifier(&payload, &["userId", "userid", "user_id"]);
        let phone = identifier(&payload, &["phone"]);
        let info = super::auth::user_info(&session)?;
        let actual = identifier(&info, &["userid", "userId", "user_id"]);
        if !uid.is_empty() && !actual.is_empty() && uid != actual {
            return Err(Error::http(
                409,
                "Loomy 本机登录态与官方账号信息不一致，请重新登录客户端",
            ));
        }
        return normalized(
            &session,
            if actual.is_empty() { &uid } else { &actual },
            &phone,
            params["name"].as_str(),
        );
    }
    let mut payload = match source["kind"].as_str() {
        Some("manual") => {
            let payload = &source["payload"];
            if let Some(index) = payload.get("methodIndex") {
                let index = index
                    .as_u64()
                    .ok_or_else(|| Error::http(400, "Loomy 登录方式无效"))?;
                if index == 4 {
                    let fields = payload["inputs"]
                        .as_object()
                        .filter(|fields| {
                            fields
                                .keys()
                                .all(|key| matches!(key.as_str(), "loginId" | "password"))
                        })
                        .ok_or_else(|| Error::http(400, "Loomy 密码登录输入无效"))?;
                    return super::auth::password(
                        &Value::Object(fields.clone()),
                        params["name"].as_str(),
                    );
                }
                if !matches!(index, 1 | 2) {
                    return Err(Error::http(400, "此登录方式不能手动提交凭据"));
                }
                let fields = payload["inputs"]
                    .as_object()
                    .filter(|fields| {
                        fields.iter().all(|(key, value)| {
                            value.is_string()
                                && if index == 1 {
                                    matches!(key.as_str(), "session" | "phone" | "userId")
                                } else {
                                    key == "credentialText"
                                }
                        })
                    })
                    .ok_or_else(|| Error::http(400, "Loomy 凭据输入无效"))?;
                if index == 2 {
                    parsed(
                        fields
                            .get("credentialText")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    )?
                } else {
                    Value::Object(fields.clone())
                }
            } else {
                payload.clone()
            }
        }
        Some("file_text") => parsed(source["content"].as_str().unwrap_or(""))?,
        _ => {
            return Err(Error::http(
                400,
                "Loomy 请使用短信、密码、登录态 JSON 或显式本机导入",
            ))
        }
    };
    if let Some(raw) = payload.as_str() {
        payload = parsed(raw)?;
    }
    let embedded = text(&payload, &["credentialText", "credentialJson"]);
    if !embedded.is_empty() {
        payload = parsed(&embedded)?;
    }
    let payload = unpack(&payload)?;
    let session = token(&payload)?;
    let uid = identifier(&payload, &["userId", "userid", "user_id"]);
    let phone = identifier(&payload, &["phone"]);
    let name = params["name"].as_str().or_else(|| payload["name"].as_str());
    if uid.is_empty() && phone.is_empty() {
        // 官方只读账号接口可补稳定身份，不消费积分或自动签到。
        let info = super::auth::user_info(&session)?;
        return normalized(
            &session,
            &identifier(&info, &["userid", "userId", "user_id"]),
            &identifier(&info, &["phone", "loginid"]),
            name,
        );
    }
    normalized(&session, &uid, &phone, name)
}
