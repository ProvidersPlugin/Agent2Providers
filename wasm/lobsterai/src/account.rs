//! 兼容旧驼峰／下划线凭证与旧身份顺序；uuid、两个 keyfrom 永远留在私密区。
//! 首次登录的 keyfrom 是续期载荷，不随续期更新。导入没有载荷的令牌只能用到失效。

use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};
use base64::{engine::general_purpose, Engine};
use sha2::{Digest, Sha256};

use super::{errors, upstream, version};

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| {
            value[*key]
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
        })
        .unwrap_or("")
        .to_string()
}

pub fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        .filter(|value: &f64| value.is_finite())
}

fn positive(value: &Value, keys: &[&str]) -> i64 {
    keys.iter()
        .find_map(|key| {
            number(&value[*key])
                .filter(|value| *value > 0.0)
                .map(|value| value as i64)
        })
        .unwrap_or(0)
}

fn user_field(value: &Value, key: &str) -> String {
    let text = text(value, &[key]);
    if !text.is_empty() {
        return text;
    }
    value[key]
        .as_i64()
        .map(|id| id.to_string())
        .or_else(|| value[key].as_u64().map(|id| id.to_string()))
        .unwrap_or_default()
}

fn user_text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .map(|key| user_field(value, key))
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

pub fn strip_bearer(value: &str) -> String {
    let mut token = value.trim();
    while token
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer "))
    {
        token = token[7..].trim();
    }
    token.to_string()
}

pub fn access(secret: &Value) -> Result<String> {
    let token = strip_bearer(&text(secret, &["accessToken", "access_token", "token"]));
    if token.is_empty() {
        return Err(errors::session("LobsterAI 账号缺少访问令牌，请重新登录"));
    }
    if token.len() > 32 * 1024
        || token
            .bytes()
            .any(|byte| !byte.is_ascii() || byte <= b' ' || byte == 127)
    {
        return Err(Error::http(400, "LobsterAI 访问令牌格式无效"));
    }
    Ok(token)
}

fn jwt_expiry(token: &str) -> i64 {
    let Some(payload) = token.split('.').nth(1) else {
        return 0;
    };
    let Ok(bytes) = general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')) else {
        return 0;
    };
    let Ok(claims) = serde_json::from_slice::<Value>(&bytes) else {
        return 0;
    };
    number(&claims["exp"])
        .filter(|value| *value > 0.0)
        .map(|value| (value * 1000.0) as i64)
        .unwrap_or(0)
}

fn timestamp(value: &Value, keys: &[&str]) -> i64 {
    let value = positive(value, keys);
    // Go 登录工具的 expiresAt 为 Unix 秒，旧网关记录为毫秒。
    if value > 0 && value < 100_000_000_000 {
        value.saturating_mul(1000)
    } else {
        value
    }
}

pub fn expires(secret: &Value) -> i64 {
    let explicit = timestamp(secret, &["expiresAt", "expires_at"]);
    if explicit > 0 {
        return explicit;
    }
    // 旧原生记录的 createdAt 是毫秒，不照搬 AtomCode 的秒口径。
    let created = timestamp(secret, &["createdAt", "created_at"]);
    let lifetime = positive(secret, &["expiresIn", "expires_in"]);
    if created > 0 && lifetime > 0 {
        return created.saturating_add(lifetime.saturating_mul(1000));
    }
    jwt_expiry(&text(secret, &["accessToken", "access_token", "token"]))
}

pub fn can_refresh(secret: &Value) -> bool {
    !text(secret, &["refreshToken", "refresh_token"]).is_empty()
        && !text(secret, &["firstKeyfrom", "first_keyfrom"]).is_empty()
        && !text(secret, &["latestKeyfrom", "latest_keyfrom"]).is_empty()
}

pub fn expiring_within(secret: &Value, min_lead_ms: i64) -> bool {
    let lead = 3_600_000_i64.max(min_lead_ms.max(0));
    can_refresh(secret)
        && expires(secret) > 0
        && expires(secret).saturating_sub(lead) <= sdk::now()
}

pub fn expiring(secret: &Value) -> bool {
    expiring_within(secret, 0)
}

pub fn identity(secret: &Value) -> String {
    let user = user_text(secret, &["userId", "user_id", "uid", "id"]);
    if !user.is_empty() {
        return user;
    }
    let token = strip_bearer(&text(secret, &["accessToken", "access_token", "token"]));
    if token.is_empty() {
        return String::new();
    }
    let tail: String = token
        .chars()
        .rev()
        .take(12)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("tok{tail}")
}

pub fn view(secret: &Value) -> Value {
    let token = strip_bearer(&text(secret, &["accessToken", "access_token", "token"]));
    let uid = user_text(secret, &["uid", "userId", "user_id"]);
    let nickname = text(secret, &["name", "nickname"]);
    let name = if !nickname.is_empty() {
        nickname.clone()
    } else if !uid.is_empty() {
        uid.clone()
    } else {
        "LobsterAI 账号".to_string()
    };
    let tail: String = token
        .chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let mut value = json!({ "name": name, "nickname": nickname, "uid": uid,
        "userId": user_text(secret, &["userId", "user_id"]), "tokenTail": tail,
        "hasSession": !token.is_empty(), "hasAccessToken": !token.is_empty(),
        "hasRefreshToken": !text(secret, &["refreshToken", "refresh_token"]).is_empty(),
        "canRefresh": can_refresh(secret) });
    if expires(secret) > 0 {
        value["expiresAt"] = json!(expires(secret));
    }
    value
}

pub fn normalized(mut secret: Value) -> Result<Value> {
    access(&secret)?;
    let id = identity(&secret);
    if id.is_empty() || id.len() > 512 || id.chars().any(char::is_control) {
        return Err(Error::http(400, "LobsterAI 账号标识无效"));
    }
    // 旧无 uid 记录按令牌尾段去重；保存此回退值，续期换 token 不得换身份。
    if user_text(&secret, &["userId", "user_id", "uid", "id"]).is_empty() {
        secret["id"] = json!(id);
    }
    Ok(json!({ "identity": id, "public": view(&secret), "secret": secret, "desktop": false }))
}

fn canonical(payload: &Value) -> Result<Value> {
    if !payload.is_object() {
        return Err(Error::http(400, "LobsterAI 凭证必须是 JSON 对象"));
    }
    let mut secret = json!({});
    for (target, aliases) in [
        ("accessToken", &["accessToken", "access_token", "token"][..]),
        ("refreshToken", &["refreshToken", "refresh_token"][..]),
        ("uid", &["uid", "yid"][..]),
        ("userId", &["userId", "user_id"][..]),
        ("id", &["id"][..]),
        ("name", &["name", "nickname"][..]),
        ("uuid", &["uuid", "UUID"][..]),
        ("firstKeyfrom", &["firstKeyfrom", "first_keyfrom"][..]),
        ("latestKeyfrom", &["latestKeyfrom", "latest_keyfrom"][..]),
    ] {
        let value = if matches!(target, "uid" | "userId" | "id") {
            user_text(payload, aliases)
        } else if target.ends_with("Token") {
            strip_bearer(&text(payload, aliases))
        } else {
            text(payload, aliases)
        };
        if value.is_empty() {
            continue;
        }
        let value = if matches!(target, "firstKeyfrom" | "latestKeyfrom") {
            normalize_keyfrom(&value)
        } else {
            value
        };
        let limit = if target.ends_with("Token") {
            32 * 1024
        } else {
            4096
        };
        if value.len() > limit
            || value.chars().any(char::is_control)
            || target.ends_with("Token")
                && (!value.is_ascii() || value.bytes().any(|byte| byte <= b' '))
        {
            return Err(Error::http(400, "LobsterAI 凭证字段过长或格式无效"));
        }
        secret[target] = json!(value);
    }
    if text(&secret, &["uid"]).is_empty() {
        let uid = user_field(&payload["user"], "id");
        let uid = if uid.is_empty() {
            user_field(&payload["user"], "yid")
        } else {
            uid
        };
        if !uid.is_empty() {
            secret["uid"] = json!(uid);
        }
    }
    for key in ["userId", "nickname"] {
        if text(&secret, &[if key == "nickname" { "name" } else { key }]).is_empty() {
            let value = user_field(&payload["user"], key);
            if !value.is_empty() {
                secret[if key == "nickname" { "name" } else { key }] = json!(value);
            }
        }
    }
    for field in ["userId", "uid", "id", "name"] {
        let value = text(&secret, &[field]);
        let limit = if field == "name" { 4096 } else { 512 };
        if value.len() > limit || value.chars().any(char::is_control) {
            return Err(Error::http(400, "LobsterAI 用户资料格式无效"));
        }
    }
    for (target, aliases) in [
        ("createdAt", &["createdAt", "created_at"][..]),
        ("expiresIn", &["expiresIn", "expires_in"][..]),
        ("expiresAt", &["expiresAt", "expires_at"][..]),
    ] {
        let value = if target == "expiresIn" {
            positive(payload, aliases)
        } else {
            timestamp(payload, aliases)
        };
        if value > 0 {
            secret[target] = json!(value);
        }
    }
    access(&secret)?;
    if secret["uuid"]
        .as_str()
        .is_some_and(|value| value.len() > 4096 || value.chars().any(char::is_control))
    {
        return Err(Error::http(400, "LobsterAI 设备信息格式无效"));
    }
    if identity(&secret).chars().any(char::is_control) {
        return Err(Error::http(400, "LobsterAI 账号标识无效"));
    }
    Ok(secret)
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
                return Err(Error::http(400, "LobsterAI 凭证文件过大"));
            }
            serde_json::from_str(content)
                .map_err(|_| Error::http(400, "LobsterAI 凭证文件不是有效 JSON"))?
        }
        Some("file_base64") => {
            let encoded = source["content"].as_str().unwrap_or("");
            if encoded.len() > (256 * 1024usize).div_ceil(3) * 4 {
                return Err(Error::http(400, "LobsterAI 凭证文件过大"));
            }
            let bytes = general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| Error::http(400, "LobsterAI 文件不是有效 base64"))?;
            if bytes.len() > 256 * 1024 {
                return Err(Error::http(400, "LobsterAI 凭证文件过大"));
            }
            serde_json::from_slice(&bytes)
                .map_err(|_| Error::http(400, "LobsterAI 凭证文件不是有效 JSON"))?
        }
        _ => {
            return Err(Error::http(
                400,
                "请使用网页登录或粘贴 LobsterAI 凭证 JSON；不读取本机文件",
            ))
        }
    };
    if let Some(raw) = payload["credentialJson"]
        .as_str()
        .filter(|text| !text.trim().is_empty())
    {
        if raw.len() > 256 * 1024 {
            return Err(Error::http(400, "LobsterAI 凭证 JSON 过大"));
        }
        payload = serde_json::from_str(raw)
            .map_err(|_| Error::http(400, "LobsterAI 凭证 JSON 格式无效"))?;
    }
    if let Some(raw) = payload.as_str() {
        payload = json!({ "accessToken": raw });
    }
    for key in ["pluginSecret", "secret", "data"] {
        if payload[key].is_object()
            && (access(&payload[key]).is_ok() || payload[key]["auth"].is_object())
        {
            payload = payload[key].clone();
            break;
        }
    }
    if payload["auth"].is_object() {
        // 公开 Go 登录工具保存 {auth, account}：只合并已知字段，auth 始终优先。
        let mut auth = payload["auth"].clone();
        if let Some(object) = auth.as_object_mut() {
            for field in ["uid", "userId", "user_id", "id", "name", "nickname"] {
                if !object.contains_key(field) {
                    if let Some(value) =
                        payload["account"].get(field).or_else(|| payload.get(field))
                    {
                        object.insert(field.into(), value.clone());
                    }
                }
            }
            if !object.contains_key("user") && payload["user"].is_object() {
                object.insert("user".into(), payload["user"].clone());
            }
        }
        payload = auth;
    }
    let mut secret = canonical(&payload)?;
    let name = text(params, &["name"]);
    if !name.is_empty() {
        secret["name"] = json!(name);
    }
    normalized(secret)
}

pub fn normalize_keyfrom(value: &str) -> String {
    let trimmed = value.trim().to_ascii_lowercase();
    let valid_channel = !trimmed.is_empty()
        && trimmed.len() <= 64
        && trimmed
            .bytes()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'));
    let legacy_timestamp =
        trimmed.len() >= 10 && trimmed.bytes().all(|byte| byte.is_ascii_digit());
    if !valid_channel || legacy_timestamp {
        upstream::OFFICIAL_KEYFROM.to_string()
    } else {
        trimmed
    }
}

pub fn login_bundle(data: &Value, uuid: &str, keyfrom: &str) -> Result<Value> {
    let mut secret = canonical(data)?;
    let normalized_keyfrom = normalize_keyfrom(keyfrom);
    secret["uuid"] = json!(uuid);
    secret["firstKeyfrom"] = json!(normalized_keyfrom);
    secret["latestKeyfrom"] = json!(normalized_keyfrom);
    secret["createdAt"] = json!(sdk::now());
    let expiry = expires(&secret);
    if expiry > 0 {
        secret["expiresAt"] = json!(expiry);
    }
    Ok(secret)
}

pub fn keyfrom_fields(secret: &Value, client_version: &str) -> Vec<(&'static str, String)> {
    let mut fields = vec![
        (
            "firstKeyfrom",
            normalize_keyfrom(&text(secret, &["firstKeyfrom", "first_keyfrom"])),
        ),
        (
            "latestKeyfrom",
            normalize_keyfrom(&text(secret, &["latestKeyfrom", "latest_keyfrom"])),
        ),
        ("version", client_version.to_string()),
    ];
    for (key, aliases) in [
        ("uuid", &["uuid", "UUID"][..]),
        ("userId", &["userId", "user_id"][..]),
    ] {
        let value = if key == "userId" {
            user_text(secret, aliases)
        } else {
            text(secret, aliases)
        };
        if !value.is_empty() {
            fields.push((key, value));
        }
    }
    fields.sort_by_key(|(key, _)| *key);
    fields
}

pub fn snapshot() -> Result<Value> {
    let record = sdk::account()?;
    if record["provider"] != "lobsterai" || sdk::account_disabled(&record) {
        return Err(Error::http(409, "LobsterAI 当前账号已停用或归属变化")
            .with_code("account_identity_changed"));
    }
    access(&record["secret"])?;
    Ok(record)
}

pub fn validate_current(expected: &Value) -> Result<()> {
    let current = snapshot()?;
    if current["id"] != expected["id"]
        || current["identity"] != expected["identity"]
        || current["secret"] != expected["secret"]
        || current["desktop"] != expected["desktop"]
    {
        return Err(
            Error::http(409, "LobsterAI 当前账号凭据已变化，请重新发起操作")
                .with_code("account_credentials_changed"),
        );
    }
    Ok(())
}

pub fn prepared() -> Result<Value> {
    ensure()?;
    snapshot()
}

pub fn ensure() -> Result<String> {
    let record = snapshot()?;
    if expiring(&record["secret"]) {
        refresh()
    } else {
        access(&record["secret"])
    }
}

pub fn fingerprint(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn refresh_fingerprint(secret: &Value) -> String {
    let fields = [
        text(secret, &["accessToken", "access_token", "token"]),
        text(secret, &["refreshToken", "refresh_token"]),
        text(secret, &["uuid", "UUID"]),
        text(secret, &["firstKeyfrom", "first_keyfrom"]),
        text(secret, &["latestKeyfrom", "latest_keyfrom"]),
        text(secret, &["userId", "user_id", "uid"]),
    ];
    fingerprint(&json!(fields).to_string())
}

pub fn refresh() -> Result<String> {
    let before = snapshot()?;
    let _lock = AccountLock::acquire()?;
    let current = snapshot()?;
    let secret = &current["secret"];
    if current["id"] != before["id"] || current["identity"] != before["identity"] {
        return Err(
            Error::http(409, "LobsterAI 续期期间账号已变化").with_code("account_identity_changed")
        );
    }
    if *secret != before["secret"] {
        return access(secret);
    }
    // 同一凭证参与续期的结果短期合并；只缓存摘要，不在会话状态复制令牌。
    let current_hash = refresh_fingerprint(secret);
    let cache_key = format!("refresh:{}", fingerprint(&text(&current, &["id"])));
    if let Ok(recent) = sdk::session_get(&cache_key) {
        if recent["credential_hash"] == current_hash {
            return access(secret);
        }
    }
    if !can_refresh(secret) {
        return Err(errors::session(
            "LobsterAI 缺少续期令牌或原始 keyfrom，请重新登录",
        ));
    }
    let client_version = version::resolve();
    validate_current(&current)?;
    let mut body = json!({});
    for (key, value) in keyfrom_fields(secret, &client_version) {
        body[key] = json!(value);
    }
    body["refreshToken"] = json!(strip_bearer(&text(
        secret,
        &["refreshToken", "refresh_token"]
    )));
    let response = upstream::anonymous("/api/auth/refresh", &body, &client_version)?;
    let data = upstream::envelope(&response, "续期")?;
    let fresh = canonical(&data)?;
    validate_current(&current)?;
    let next_token = access(&fresh)?;
    let old_user = user_text(secret, &["userId", "user_id"]);
    let new_user = user_text(&fresh, &["userId"]);
    if !old_user.is_empty() && !new_user.is_empty() && old_user != new_user {
        return Err(errors::session(
            "LobsterAI 续期返回了不同的账号身份，未覆盖现有凭证",
        ));
    }
    let mut next = secret.clone();
    let previous_identity = identity(secret);
    let old_uid = user_text(secret, &["uid"]);
    let new_uid = user_text(&fresh, &["uid"]);
    if !old_uid.is_empty() && !new_uid.is_empty() && old_uid != new_uid {
        return Err(errors::session("LobsterAI 续期账号不一致，未写入新凭证"));
    }
    if user_text(secret, &["userId", "user_id", "uid", "id"]).is_empty() {
        next["id"] = json!(previous_identity);
    }
    next["accessToken"] = json!(next_token);
    next["firstKeyfrom"] = json!(normalize_keyfrom(&text(
        secret,
        &["firstKeyfrom", "first_keyfrom"]
    )));
    next["latestKeyfrom"] = json!(normalize_keyfrom(&text(
        secret,
        &["latestKeyfrom", "latest_keyfrom"]
    )));
    let refreshed = text(&fresh, &["refreshToken"]);
    if !refreshed.is_empty() {
        next["refreshToken"] = json!(refreshed);
    }
    let lifetime = positive(&fresh, &["expiresIn"]);
    let now = sdk::now();
    next["createdAt"] = json!(now);
    next["expiresIn"] = json!(lifetime);
    let explicit = positive(&fresh, &["expiresAt"]);
    let expiry = if explicit > 0 {
        explicit
    } else if lifetime > 0 {
        now.saturating_add(lifetime.saturating_mul(1000))
    } else {
        jwt_expiry(&next_token)
    };
    next["expiresAt"] = json!(expiry);
    // 不保留冲突的旧别名；uuid 与 keyfrom 不从续期回应里更新。
    if let Some(object) = next.as_object_mut() {
        for key in [
            "access_token",
            "token",
            "refresh_token",
            "expires_at",
            "expires_in",
            "created_at",
            "first_keyfrom",
            "latest_keyfrom",
            "user",
            "auth",
        ] {
            object.remove(key);
        }
        if refreshed.is_empty() {
            object.insert(
                "refreshToken".into(),
                json!(text(secret, &["refreshToken", "refresh_token"])),
            );
        }
    }
    let updated = sdk::host(
        "account.update",
        json!({ "expect": secret, "secret": next, "public": view(&next) }),
    )?;
    if updated["updated"] != true {
        let latest = snapshot()?;
        if latest["id"] != current["id"] || latest["identity"] != current["identity"] {
            return Err(Error::http(409, "LobsterAI 续期结果未写回原账号")
                .with_code("account_identity_changed"));
        }
        return access(&latest["secret"]);
    }
    let _ = sdk::session_put(
        &cache_key,
        json!({ "credential_hash": refresh_fingerprint(&next) }),
        5000,
    );
    Ok(next_token)
}
