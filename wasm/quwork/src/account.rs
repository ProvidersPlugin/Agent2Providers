//! 同原生 QuWork 的 JSON／DSH 文本凭证解析与身份规则；不把 accessToken 当 llmToken。
//! 桌面标记只由显式导入流程创建，粘贴 source:desktop 不会触发本机读取。

use super::{client, desktop, endpoints};
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

pub fn user_view(user: &Value) -> Value {
    let mut public = json!({});
    for (target, keys) in [
        ("uid", &["id", "uid", "userId"][..]),
        ("username", &["username"][..]),
        ("nickname", &["display_name", "displayName", "nickname"][..]),
        ("phone", &["phone"][..]),
    ] {
        if let Some(value) = keys.iter().find_map(|key| {
            user.get(*key).filter(|value| {
                value.is_number() || value.as_str().is_some_and(|text| !text.trim().is_empty())
            })
        }) {
            let value = value
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| value.to_string());
            public[target] = json!(value
                .chars()
                .filter(|ch| !ch.is_control())
                .take(256)
                .collect::<String>());
        }
    }
    public
}

pub fn token(value: &str) -> Result<String> {
    let mut value = value.trim();
    while value
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer"))
    {
        let rest = &value[6..];
        if !rest.starts_with(char::is_whitespace) {
            break;
        }
        value = rest.trim_start();
    }
    if value.len() > 16_384
        || !value.is_ascii()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(Error::http(400, "QuWork 凭证格式不正确"));
    }
    Ok(value.to_string())
}

pub fn parse(value: &Value) -> Result<Value> {
    parse_inner(value, 0)
}

fn parse_inner(value: &Value, depth: usize) -> Result<Value> {
    if depth > 6 {
        return Err(Error::http(400, "QuWork 凭证嵌套过深"));
    }
    if value.to_string().len() > endpoints::MAX_CREDENTIALS {
        return Err(Error::http(400, "QuWork 凭证内容超过大小限制"));
    }
    if let Some(content) = value.as_str() {
        return parse_text_inner(content, depth + 1);
    }
    if !value.is_object() {
        return Err(Error::http(400, "QuWork 凭证需要 JSON 对象或模型凭证文本"));
    }
    let mut input = value.clone();
    for key in ["pluginSecret", "secret", "session", "data"] {
        if input[key].is_object() {
            input = input[key].clone();
        }
    }
    let nested = input["llmToken"]
        .as_str()
        .filter(|value| value.trim().starts_with('{'));
    let mut parsed = if let Some(content) = input["credentials"]
        .as_str()
        .or(nested)
        .filter(|value| !value.trim().is_empty())
    {
        parse_text_inner(content, depth + 1)?
    } else {
        json!({})
    };
    let access = token(&text(&input, &["accessToken", "access_token", "token"]))?;
    let llm = if nested.is_some() {
        String::new()
    } else {
        token(&text(&input, &["llmToken", "llm_token", "apiKey", "key"]))?
    };
    if !access.is_empty() {
        parsed["accessToken"] = json!(access);
    }
    if !llm.is_empty() {
        parsed["llmToken"] = json!(llm);
    }
    if input["user"].is_object() {
        parsed["user"] = user_view(&input["user"]);
    } else if input.get("uid").is_some() {
        parsed["user"] = user_view(&input);
    }
    if let Some(expires) = client::timestamp(&input["expiresAt"])
        .or_else(|| client::timestamp(&input["expire"]))
        .or_else(|| jwt_expiry(&text(&parsed, &["accessToken"])))
    {
        parsed["expiresAt"] = json!(expires);
    }
    if let Some(updated) = client::timestamp(&input["updatedAt"]) {
        parsed["updatedAt"] = json!(updated);
    }
    for key in ["name", "deviceIdentifier", "deviceUuid"] {
        let value = text(&input, &[key]);
        if !value.is_empty() {
            if value.len() > 256
                || value.chars().any(char::is_control)
                || key == "deviceIdentifier" && !valid_device(&value)
            {
                return Err(Error::http(400, "QuWork 账号或设备标识格式不正确"));
            }
            parsed[key] = json!(value);
        }
    }
    if text(&parsed, &["llmToken"]).is_empty() {
        return Err(Error::http(
            400,
            "缺少 QuWork 模型凭证 llmToken；登录凭证不能代替模型凭证",
        ));
    }
    Ok(parsed)
}

pub fn parse_text(content: &str) -> Result<Value> {
    parse_text_inner(content, 0)
}

fn parse_text_inner(content: &str, depth: usize) -> Result<Value> {
    if depth > 6 {
        return Err(Error::http(400, "QuWork 凭证嵌套过深"));
    }
    if content.len() > endpoints::MAX_CREDENTIALS {
        return Err(Error::http(400, "QuWork 凭证内容超过大小限制"));
    }
    let content = content.trim().trim_start_matches('\u{feff}').trim();
    if content.starts_with('{') {
        let value: Value = serde_json::from_str(content)
            .map_err(|_| Error::http(400, "QuWork 凭证 JSON 格式不正确"))?;
        return parse_inner(&value, depth + 1);
    }
    if let Some(value) = content
        .lines()
        .find_map(|line| line.trim().strip_prefix("QURIVS_LLM_API_KEY:"))
    {
        let value = value.trim();
        let value = if value.starts_with('"') {
            serde_json::from_str::<String>(value)
                .map_err(|_| Error::http(400, "QuWork 模型凭证文本格式不正确"))?
        } else {
            value.trim_matches('\'').to_string()
        };
        return parse_inner(&json!({ "llmToken": value }), depth + 1);
    }
    parse_inner(&json!({ "llmToken": content }), depth + 1)
}

pub fn jwt_expiry(token: &str) -> Option<i64> {
    let bytes = URL_SAFE_NO_PAD
        .decode(token.split('.').nth(1)?.trim_end_matches('='))
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    client::timestamp(&claims["exp"])
}

pub fn fingerprint(secret: &str) -> String {
    // 原生 flight::fingerprint 是 SHA-256 前16字节，而非完整64位摘要。
    Sha256::digest(secret.as_bytes())
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn identity(secret: &Value) -> String {
    let uid = text(&secret["user"], &["uid", "id"]);
    if !uid.is_empty() {
        return format!("quwork:{uid}");
    }
    let llm = text(secret, &["llmToken"]);
    if !llm.is_empty() {
        return format!("quwork:key:{}", fingerprint(&llm));
    }
    let fingerprint = text(secret, &["modelFingerprint"]);
    if fingerprint.is_empty() {
        String::new()
    } else {
        format!("quwork:key:{fingerprint}")
    }
}

pub fn unchanged(expected: &Value) -> Result<()> {
    let current = agent2api_wasm_sdk::account()?;
    if ["id", "provider", "identity", "desktop", "secret"]
        .iter()
        .any(|key| current[*key] != expected[*key])
    {
        return Err(Error::http(409, "QuWork 账号在操作期间已变化，请重新发起")
            .with_code("identity_changed"));
    }
    Ok(())
}

pub fn local(secret: &Value) -> bool {
    secret["source"] == "desktop"
}
pub fn valid_device(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn view(secret: &Value) -> Value {
    let user = &secret["user"];
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
    let local = local(secret);
    let has_login = !text(secret, &["accessToken"]).is_empty() || secret["hasLoginToken"] == true;
    let tail = if llm.is_empty() {
        text(secret, &["tokenTail"])
    } else {
        llm.chars()
            .rev()
            .take(6)
            .collect::<String>()
            .chars()
            .rev()
            .collect()
    };
    let mut view = json!({ "name": if !name.is_empty() { &name } else if !nickname.is_empty() { &nickname } else { "QuWork" },
        "uid": text(user, &["uid", "id"]), "nickname": nickname, "phone": text(user, &["phone"]),
        "hasToken": !llm.is_empty() || local, "available": !llm.is_empty() || local,
        "hasAccessToken": has_login, "hasRefreshToken": false, "canRefresh": has_login,
        "tokenTail": tail.chars().rev().take(6).collect::<String>().chars().rev().collect::<String>(),
        "desktopAuth": local });
    // hasLoginToken 是旧私密引用字段；WASM 公开字段白名单不允许它，改用 hasAccessToken。
    for key in ["expiresAt", "updatedAt"] {
        if let Some(value) = client::timestamp(&secret[key]) {
            view[key] = json!(value);
        }
    }
    view
}

pub fn normalized(mut secret: Value, name: Option<&str>, local: bool) -> Result<Value> {
    if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
        secret["name"] = json!(name.chars().take(128).collect::<String>());
    }
    let identity = identity(&secret);
    if identity.is_empty() || identity.len() > 512 || identity.chars().any(char::is_control) {
        return Err(Error::http(400, "QuWork 账号身份缺失或格式无效"));
    }
    Ok(json!({ "identity": identity, "public": view(&secret), "secret": secret, "desktop": local }))
}

pub fn normalize(params: &Value) -> Result<Value> {
    let source = &params["source"];
    let local = source["kind"] == "desktop";
    let mut secret = match source["kind"].as_str() {
        Some("desktop") => desktop::reference()?,
        Some("manual") => {
            let payload = &source["payload"];
            if payload
                .get("methodIndex")
                .is_some_and(|index| index.as_u64() != Some(1))
            {
                return Err(Error::http(400, "QuWork 凭据导入方式无效，请重新选择"));
            }
            parse(if payload["inputs"].is_object() {
                &payload["inputs"]
            } else {
                payload
            })?
        }
        Some("file_text") => parse_text(source["content"].as_str().unwrap_or(""))?,
        _ => {
            return Err(Error::http(
                400,
                "QuWork 只接受显式本机导入、JSON 或 .credentials.yaml 文本，不接受二进制文件",
            ))
        }
    };
    if !local {
        let access = text(&secret, &["accessToken"]);
        if !access.is_empty() {
            let user = client::request("GET", endpoints::SELF, &access, None)?.data("账号查询")?;
            if !user.is_object() {
                return Err(Error::http(502, "QuWork 账号信息格式异常"));
            }
            secret["user"] = user_view(&user);
        }
    }
    normalized(secret, params["name"].as_str(), local)
}
