//! refreshToken 为主凭据；accessToken 可空，临期提前15分钟。兼容旧原生字段包装。

use super::util;
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn fields(mut value: &Value) -> &Value {
    for _ in 0..4 {
        let next = ["pluginSecret", "secret", "auth"]
            .iter()
            .find_map(|key| value.get(*key).filter(|v| v.is_object()));
        match next {
            Some(next) => value = next,
            None => break,
        }
    }
    value
}

pub fn parse(value: &Value) -> Result<Value> {
    let value = fields(value);
    if !value.is_object() {
        return Err(Error::http(401, "Antigravity 已保存凭据格式无效"));
    }
    let refresh = util::header(
        &util::text(value, &["refreshToken", "refresh_token"]),
        16_384,
    )?;
    if refresh.is_empty() {
        return Err(Error::http(401, "Antigravity 缺少续期凭据，请重新登录")
            .with_code("antigravity_credentials"));
    }
    let mut result = value.clone();
    result["refreshToken"] = json!(refresh);
    for (target, aliases, limit) in [
        ("accessToken", &["accessToken", "access_token"][..], 16_384),
        ("projectId", &["projectId", "project_id"][..], 1024),
        ("email", &["email", "userId", "user_id"][..], 480),
    ] {
        let text = util::header(&util::text(value, aliases), limit)?;
        if !text.is_empty() {
            result[target] = json!(text);
        }
    }
    if let Some(at) = expiry(value) {
        result["expiresAt"] = json!(at);
    }
    Ok(result)
}

pub fn expiry(value: &Value) -> Option<i64> {
    ["expiresAt", "expires_at"]
        .iter()
        .find_map(|key| util::integer(&value[*key]))
        .filter(|at| *at > 0)
}
pub fn needs_refresh(secret: &Value, now: i64) -> bool {
    util::text(secret, &["accessToken"]).is_empty()
        || expiry(secret).is_some_and(|at| at.saturating_sub(900_000) <= now)
}
pub fn token(secret: &Value) -> Result<String> {
    let token = util::header(&util::text(secret, &["accessToken"]), 16_384)?;
    if token.is_empty() {
        return Err(
            Error::http(401, "Antigravity 没有可用的访问令牌，请续期或重新授权")
                .with_code("antigravity_credentials"),
        );
    }
    Ok(token)
}
pub fn binding(secret: &Value) -> String {
    util::digest(
        json!([
            "antigravity",
            util::text(secret, &["refreshToken"]),
            util::text(secret, &["projectId"])
        ])
        .to_string()
        .as_bytes(),
    )
}
pub fn enterprise(secret: &Value) -> bool {
    let email = util::text(secret, &["email"]).to_ascii_lowercase();
    !email.is_empty() && !email.ends_with("@gmail.com") && !email.ends_with("@googlemail.com")
}

fn merge_at(value: &mut Value, patch: &Value, depth: usize) -> Result<()> {
    if depth < 4 {
        if let Some(key) = ["pluginSecret", "secret", "auth"]
            .into_iter()
            .find(|key| value.get(*key).is_some_and(Value::is_object))
        {
            return merge_at(&mut value[key], patch, depth + 1);
        }
    }
    let object = value
        .as_object_mut()
        .ok_or_else(|| Error::http(409, "Antigravity 凭据不能回写"))?;
    for (key, next) in patch.as_object().into_iter().flatten() {
        if !next.is_null() && next.as_str() != Some("") {
            object.insert(key.clone(), next.clone());
        }
    }
    Ok(())
}
pub fn merge(secret: &Value, patch: &Value) -> Result<Value> {
    let mut next = secret.clone();
    merge_at(&mut next, patch, 0)?;
    Ok(next)
}
