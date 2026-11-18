//! 旧平铺/CPA {auth,account} 字段归一；读取与续期不能丢失原设备密钥。
//! variant 决定 OAuth 归属候选，不决定聊天 function；Intl 一律在入口拒绝。

use super::{device, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

pub const LEAD_MS: i64 = 24 * 3600 * 1000;
pub const MAX_AGE_MS: i64 = 15 * 24 * 3600 * 1000;

pub fn variant(secret: &Value) -> &str {
    if secret["variant"] == "cn" {
        "cn"
    } else {
        "solo"
    }
}

pub fn check_variant(value: &str) -> Result<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "intl" | "solo-intl" => Err(Error::http(
            400,
            "Trae 国际版需要独立身份与目录，不能使用国内提供商",
        )),
        "cn" => Ok("cn"),
        _ => Ok("solo"),
    }
}

pub fn parse(payload: &Value) -> Result<Value> {
    let mut payload = payload;
    for key in ["pluginSecret", "secret"] {
        if payload[key].is_object() {
            payload = &payload[key];
        }
    }
    let auth = payload
        .get("auth")
        .filter(|v| v.is_object())
        .unwrap_or(payload);
    // 旧导入可能把资料或设备字段留在顶层；嵌套已给实值时始终优先，
    // 缺失才取同名顶层值，不能把旧顶层令牌覆盖新auth令牌。
    let pick = |keys: &[&str]| {
        let value = util::text(auth, keys);
        if value.is_empty() {
            util::text(payload, keys)
        } else {
            value
        }
    };
    let empty = Value::Null;
    let profile = payload
        .get("account")
        .filter(|v| v.is_object())
        .unwrap_or(&empty);
    if !auth.is_object() {
        return Err(Error::http(400, "Trae 凭据必须是 JSON 对象"));
    }
    let mut out = json!({ "variant": check_variant(&pick(&["variant"]))? });
    for field in [
        "accessToken",
        "refreshToken",
        "machineId",
        "deviceId",
        "authClientId",
    ] {
        let text = if field == "authClientId" {
            pick(&["authClientId", "ClientID", "clientId"])
        } else {
            pick(&[field])
        };
        if !text.is_empty() {
            out[field] = json!(util::header(
                &text,
                if field.ends_with("Token") { 16384 } else { 256 }
            )?);
        }
    }
    for field in [
        "accessToken",
        "refreshToken",
        "machineId",
        "deviceId",
        "authClientId",
        "devicePublicKey",
        "devicePrivateKey",
    ] {
        if [auth.get(field), payload.get(field)]
            .into_iter()
            .flatten()
            .any(|v| !v.is_null() && !v.is_string())
        {
            return Err(Error::http(400, "Trae 凭据、设备标识和密钥必须是文本"));
        }
    }
    if util::text(&out, &["accessToken", "refreshToken"]).is_empty() {
        return Err(Error::http(
            400,
            "Trae 凭据缺少 accessToken 或 refreshToken",
        ));
    }
    let host = pick(&["apiHost"]);
    if !host.is_empty() {
        out["apiHost"] = json!(util::account_origin(&host)?);
    }
    let domain = pick(&["domain"]);
    if !domain.is_empty() {
        let domain = domain.trim_start_matches("https://").trim_end_matches('/');
        if !matches!(domain, "trae.cn" | "trae.com.cn") {
            return Err(Error::http(400, "Trae 凭据不属于已接入的国内服务"));
        }
        out["domain"] = json!(domain);
    }
    for field in ["uid", "enterpriseId", "nickname"] {
        let value = [profile.get(field), auth.get(field), payload.get(field)]
            .into_iter()
            .flatten()
            .map(util::scalar)
            .find(|v| !v.is_empty())
            .unwrap_or_default();
        if value.len() > 480 || value.chars().any(char::is_control) {
            return Err(Error::http(400, "Trae 账号资料格式无效"));
        }
        if !value.is_empty() {
            out[field] = json!(value);
        }
    }
    let public = pick(&["devicePublicKey"]);
    let private = pick(&["devicePrivateKey"]);
    device::validate(&public, &private)?;
    if !public.is_empty() {
        out["devicePublicKey"] = json!(public);
        out["devicePrivateKey"] = json!(private);
    }
    if let Some(at) = ["expiresAt", "expiresAtMs", "TokenExpireAt", "tokenExpireAt"]
        .iter()
        .find_map(|key| util::millis(&auth[*key]))
        .or_else(|| {
            ["expiresAt", "expiresAtMs", "TokenExpireAt", "tokenExpireAt"]
                .iter()
                .find_map(|key| util::millis(&payload[*key]))
        })
    {
        out["expiresAt"] = json!(at);
    }
    for field in ["name", "source"] {
        let value = util::text(payload, &[field]);
        if !value.is_empty() {
            out[field] = json!(util::safe_text(&value, 100));
        }
    }
    Ok(out)
}

fn jwt(secret: &Value) -> Option<Value> {
    let token = secret["accessToken"].as_str()?;
    let payload = token.split('.').nth(1)?.trim_end_matches('=');
    if payload.len() > 24 * 1024 {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn expiry(secret: &Value) -> Option<i64> {
    let claims = jwt(secret).unwrap_or(Value::Null);
    ["exp", "TokenExpireAt", "tokenExpireAt"]
        .iter()
        .find_map(|field| util::millis(&claims[*field]))
        .or_else(|| util::millis(&secret["expiresAt"]))
}

pub fn needs_refresh(secret: &Value, now: i64) -> bool {
    if util::text(secret, &["accessToken"]).is_empty() {
        return true;
    }
    expiry(secret).is_some_and(|at| at.saturating_sub(now) <= LEAD_MS)
        || jwt(secret)
            .and_then(|claims| util::integer(&claims["iat"]))
            .filter(|at| *at > 0)
            .is_some_and(|at| now.saturating_sub(at.saturating_mul(1000)) >= MAX_AGE_MS)
}

pub fn token(secret: &Value) -> Result<String> {
    check_variant(&util::text(secret, &["variant"]))?;
    let token = util::header(&util::text(secret, &["accessToken"]), 16384)?;
    if token.is_empty() {
        return Err(super::errors::expired());
    }
    if let Some(at) = expiry(secret) {
        if at <= util::now()? {
            return Err(super::errors::expired());
        }
    }
    Ok(token)
}

pub fn binding(secret: &Value) -> String {
    // 换凭据后的旧目录不能授权新token；正常续期后下一轮按当前账号重取。
    util::digest(
        json!([
            variant(secret),
            secret["uid"],
            secret["deviceId"],
            secret["machineId"],
            util::text(secret, &["accessToken"])
        ])
        .to_string()
        .as_bytes(),
    )
}
