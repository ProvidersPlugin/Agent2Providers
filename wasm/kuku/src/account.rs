//! 保持原生kuku-<uid>与缺uid时BDUSS尾8位身份；公开tokenTail遵守宿主最多6位。
//! 手工/文本导入必须userreport即时验证。桌面导入原生为快照语义，不新增实时读盘。

use crate::{credentials::Credentials, errors, session, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn identity(secret: &Value) -> Result<String> {
    let c = Credentials::from_secret(secret)?;
    let uid = c.uid.trim();
    let key = if !uid.is_empty() {
        uid.to_string()
    } else {
        let old = util::text(secret, &["identityTail", "tokenTail"]);
        if old.is_empty() {
            c.original_tail()
        } else {
            old
        }
    };
    if key.is_empty()
        || key.len() > 480
        || key.chars().any(char::is_control)
        || errors::redact(&key, &c.secrets()) != key
    {
        return Err(Error::http(400, "库库缺少安全稳定的账号身份"));
    }
    Ok(format!("kuku-{key}"))
}
pub fn view(secret: &Value) -> Result<Value> {
    let c = Credentials::from_secret(secret)?;
    let explicit = util::text(secret, &["name"]);
    let name = if !explicit.is_empty() {
        explicit
    } else if c.uid.is_empty() {
        "KukuAI 账号".into()
    } else {
        format!("库库 AI {}", c.uid)
    };
    let mut public = errors::sanitize(
        &json!({ "name": name, "uid": c.uid, "loginName": c.nickname,
        "hasSession": true, "canRefresh": false, "source": util::text(secret, &["source"]),
        "desktop": secret["source"] == "desktop", "available": true }),
        &c.secrets(),
    );
    let tail = c
        .get("BDUSS")
        .chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    public["tokenTail"] = json!(if c.get("BDUSS").len() > 8
        && errors::redact(&tail, &c.secrets()) == tail
    {
        tail
    } else {
        String::new()
    });
    Ok(public)
}
fn input(params: &Value) -> Result<(Credentials, &'static str, Option<String>)> {
    let source = &params["source"];
    match source["kind"].as_str() {
        Some("desktop") => Ok((
            Credentials::from_value(&agent2api_wasm_sdk::native_call(
                "desktop.credentials.kuku.v1",
                "read",
                json!({}),
            )?)?,
            "desktop",
            None,
        )),
        Some("file_base64") => Ok((
            Credentials::from_value(&agent2api_wasm_sdk::host("resource.cookie-file.take", json!({}))?)?,
            "file",
            None,
        )),
        Some("file_text") => {
            let text = source["content"].as_str().unwrap_or("");
            if text.len() > 128 * 1024 {
                return Err(Error::http(400, "库库凭据文件过大"));
            }
            let value: Value = serde_json::from_str(text).map_err(|_| {
                Error::http(
                    400,
                    "库库文本导入只接受单账号JSON，Cookies数据库请使用文件导入",
                )
            })?;
            Ok((Credentials::from_value(&value)?, "file", None))
        }
        Some("manual") => {
            let payload = &source["payload"];
            if let Some(index) = payload.get("methodIndex") {
                if payload
                    .get("provider")
                    .and_then(Value::as_str)
                    .is_some_and(|provider| provider != "kuku")
                {
                    return Err(Error::http(400, "登录材料不属于库库"));
                }
                if !matches!(index.as_u64(), Some(1 | 2)) {
                    return Err(Error::http(400, "请选择Cookie或文本JSON导入"));
                }
                let fields = payload["inputs"]
                    .as_object()
                    .ok_or_else(|| Error::http(400, "库库登录字段须为对象"))?;
                let key = if index == &json!(1) {
                    "cookie"
                } else {
                    "credentialJson"
                };
                if fields.iter().any(|(name, value)| {
                    ![key, "name"].contains(&name.as_str()) || !value.is_string()
                }) {
                    return Err(Error::http(400, "库库登录包含未申报字段"));
                }
                let raw = fields.get(key).and_then(Value::as_str).unwrap_or("");
                let c = Credentials::from_value(&json!(raw))?;
                return Ok((
                    c,
                    "manual",
                    fields
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                ));
            }
            Ok((Credentials::from_value(payload)?, "manual", None))
        }
        _ => Err(Error::http(400, "库库请使用Cookie或文本JSON导入")),
    }
}
pub fn normalize(params: &Value) -> Result<Value> {
    let (c, source, form_name) = input(params)?;
    let verified = session::verify(c)?;
    let name = params["name"]
        .as_str()
        .filter(|name| !name.trim().is_empty())
        .map(str::to_string)
        .or(form_name)
        .unwrap_or_else(|| {
            if !verified.nickname.is_empty() {
                verified.nickname.clone()
            } else if !verified.uid.is_empty() {
                format!("库库 AI {}", verified.uid)
            } else {
                "KukuAI 账号".into()
            }
        });
    let secret = json!({ "accessToken": verified.cookie(), "uid": verified.uid,
        "loginName": verified.nickname, "source": source, "identityTail": verified.original_tail(),
        "tokenTail": verified.original_tail(), "name": util::safe_text(&errors::redact(&name, &verified.secrets()), 64) });
    Ok(
        json!({ "identity": identity(&secret)?, "secret": secret, "public": view(&secret)?, "desktop": source == "desktop" }),
    )
}
