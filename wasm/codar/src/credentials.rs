use agent2api_wasm_sdk::{json, Error, Result, Value};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

use crate::util;

pub fn token(raw: &str) -> Result<String> {
    let raw = raw.trim();
    let raw = if raw
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer "))
    {
        raw.get(7..).unwrap_or("").trim()
    } else {
        raw
    };
    if raw.len() > 8192
        || raw.is_empty()
        || !raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    {
        return Err(Error::http(400, "请填写科搭签发的完整访问令牌 JWT"));
    }
    let parts = raw.split('.').collect::<Vec<_>>();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
        return Err(Error::http(
            400,
            "科搭访问令牌必须是完整 JWT，不能包含多个账号",
        ));
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(parts[1].trim_end_matches('='))
        .map_err(|_| Error::http(400, "科搭令牌格式无效"))?;
    let payload: Value =
        serde_json::from_slice(&decoded).map_err(|_| Error::http(400, "科搭令牌内容无效"))?;
    if !payload.is_object() {
        return Err(Error::http(400, "科搭令牌内容不是有效对象"));
    }
    Ok(raw.to_string())
}

pub fn claims(token: &str) -> Result<Value> {
    let part = token
        .split('.')
        .nth(1)
        .ok_or_else(|| Error::http(401, "科搭登录凭据无效"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(part.trim_end_matches('='))
        .map_err(|_| Error::http(401, "科搭登录凭据无效"))?;
    serde_json::from_slice(&bytes).map_err(|_| Error::http(401, "科搭登录凭据无效"))
}

pub fn expires(token: &str) -> Option<i64> {
    claims(token)
        .ok()
        .and_then(|claims| util::timestamp(&claims["exp"]))
}

fn direct(value: &Value) -> Result<Value> {
    direct_at(value, 0)
}

fn direct_at(value: &Value, depth: usize) -> Result<Value> {
    if depth > 8 {
        return Err(Error::http(400, "科搭凭据 JSON 嵌套过深"));
    }
    if let Some(raw) = value.as_str() {
        if raw.len() > 256 * 1024 {
            return Err(Error::http(400, "科搭凭据文本超过256 KiB限制"));
        }
        let text = raw.trim();
        if text.starts_with('{') || text.starts_with('[') {
            let parsed = serde_json::from_str(text)
                .map_err(|_| Error::http(400, "科搭凭据 JSON 格式无效"))?;
            return direct_at(&parsed, depth + 1);
        }
        return Ok(json!({ "accessToken": token(text)? }));
    }
    if !value.is_object() || value.to_string().len() > 256 * 1024 || value.get("accounts").is_some()
    {
        return Err(Error::http(400, "请导入单个科搭账号，不要导入整个账号池"));
    }
    if let Some(text) = value.get("credentialJson").and_then(Value::as_str) {
        return direct_at(&json!(text), depth + 1);
    }
    reject_local(value)?;
    let mut candidates = Vec::new();
    for object in [value, &value["secret"], &value["provider"]["speedai"]] {
        for key in ["accessToken", "access_token", "token"] {
            if let Some(raw) = object[key]
                .as_str()
                .filter(|value| !value.trim().is_empty())
            {
                candidates.push(token(raw)?);
            }
        }
    }
    candidates.sort();
    candidates.dedup();
    if candidates.len() != 1 {
        return Err(Error::http(
            400,
            "凭据文件没有唯一的科搭访问令牌，请选择一个账号",
        ));
    }
    let mut result = json!({ "accessToken": candidates.remove(0) });
    let mut identity_hint = None;
    for object in [value, &value["secret"], &value["provider"]["speedai"]] {
        if object.is_object() {
            reject_local(object)?;
        }
        for key in ["uid", "username", "name"] {
            if let Some(text) = if key == "uid" {
                util::identifier(&object[key])
            } else {
                let text = util::text(object, &[key]);
                (!text.is_empty()).then_some(text)
            } {
                if key == "username" {
                    if identity_hint.as_ref().is_some_and(|known| known != &text) {
                        return Err(Error::http(400, "科搭凭据 JSON 中的账号名不一致"));
                    }
                    identity_hint = Some(text.clone());
                }
                if result.get(key).is_none() {
                    result[key] = json!(text);
                }
            }
        }
    }
    Ok(result)
}

fn reject_local(value: &Value) -> Result<()> {
    if value["source"] == "desktop"
        || value["source"]["kind"] == "desktop"
        || value["desktop"] == true
        || value["desktopReadOnly"] == true
        || value.get("desktopProfile").is_some()
    {
        return Err(Error::http(
            400,
            "本机科搭关联必须从本机导入入口创建，不能由手工 JSON 授权读盘",
        ));
    }
    Ok(())
}

pub fn input(params: &Value) -> Result<Value> {
    match params["source"]["kind"].as_str() {
        Some("file_text") => direct(&params["source"]["content"]),
        Some("manual") => {
            let payload = &params["source"]["payload"];
            if let Some(index) = payload["methodIndex"].as_u64() {
                if !matches!(index, 2 | 3) {
                    return Err(Error::http(400, "请使用科搭网页登录或本机关联入口"));
                }
                let fields = payload["inputs"]
                    .as_object()
                    .ok_or_else(|| Error::http(400, "科搭凭据字段无效"))?;
                let key = if index == 2 {
                    "token"
                } else {
                    "credentialJson"
                };
                if fields.iter().any(|(field, value)| {
                    ![key, "name"].contains(&field.as_str()) || !value.is_string()
                }) {
                    return Err(Error::http(400, "科搭凭据包含未声明字段"));
                }
                let mut result = direct(
                    fields
                        .get(key)
                        .ok_or_else(|| Error::http(400, "请填写科搭凭据"))?,
                )?;
                if let Some(name) = fields.get("name") {
                    result["name"] = name.clone();
                }
                Ok(result)
            } else {
                direct(if payload["inputs"].is_object() {
                    &payload["inputs"]
                } else {
                    payload
                })
            }
        }
        _ => Err(Error::http(
            400,
            "科搭支持访问令牌、文本 JSON 和经明确授权的本机关联，不读取任意文件路径",
        )),
    }
}
