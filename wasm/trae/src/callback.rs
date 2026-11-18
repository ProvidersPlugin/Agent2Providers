//! 官方无state回调必须由宿主每flow真实listener归属；这里只验证原始URL与AuthCode。
//! refreshToken回调不能证明PKCE，必须改走已鉴权管理表单手工导入，绝不自动落账号。

use super::{endpoints, profile, util};
use agent2api_wasm_sdk::{Error, Result, Value};
use std::collections::BTreeMap;

pub struct Callback {
    pub code: String,
    pub login_host: String,
    pub profile: Value,
    pub denied: bool,
}

pub fn receiver(raw: &str) -> Result<url::Url> {
    let parsed = url::Url::parse(raw).map_err(|_| Error::http(400, "Trae 回调地址格式无效"))?;
    if parsed.scheme() != "http"
        || parsed.host_str() != Some("127.0.0.1")
        || parsed.port().is_none_or(|p| p == 0)
        || parsed.path() != endpoints::CALLBACK_PATH
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
        || parsed.query().is_some()
        || parsed.as_str() != raw
    {
        return Err(Error::http(
            400,
            "Trae 必须使用主程序已绑定的127.0.0.1随机端口与/authorize路径",
        ));
    }
    Ok(parsed)
}

fn conflicting() -> Error {
    Error::http(400, "Trae 回调包含冲突或无效参数，请重新授权")
}

fn unique(map: &BTreeMap<String, String>, keys: &[&str]) -> Result<String> {
    let mut found: Option<&str> = None;
    for key in keys {
        if let Some(value) = map.get(*key).map(String::as_str).filter(|v| !v.is_empty()) {
            if found.is_some_and(|old| old != value) {
                return Err(conflicting());
            }
            found = Some(value);
        }
    }
    Ok(found.unwrap_or("").to_string())
}

fn nested_code(value: &Value, depth: usize, out: &mut Option<String>) -> Result<()> {
    if depth > 8 {
        return Err(conflicting());
    }
    match value {
        Value::Object(object) => {
            for key in ["authCode", "auth_code", "AuthCode", "AuthCodeToken", "code"] {
                if let Some(text) = object
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|v| !v.trim().is_empty())
                {
                    if out.as_ref().is_some_and(|old| old != text) {
                        return Err(conflicting());
                    }
                    *out = Some(text.into());
                }
            }
            for value in object.values().filter(|v| v.is_object() || v.is_array()) {
                nested_code(value, depth + 1, out)?;
            }
        }
        Value::Array(list) => {
            for value in list {
                nested_code(value, depth + 1, out)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn profile_json(raw: &str) -> Value {
    let mut value: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return Value::Null,
    };
    for _ in 0..2 {
        let Some(text) = value.as_str() else {
            break;
        };
        value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => return Value::Null,
        };
    }
    if value.is_object() {
        profile::project(&value)
    } else {
        Value::Null
    }
}

pub fn parse(raw: &str, expected: &str) -> Result<Callback> {
    receiver(expected)?;
    if raw.len() > 32 * 1024 || raw.chars().any(char::is_control) {
        return Err(conflicting());
    }
    let url = url::Url::parse(raw).map_err(|_| conflicting())?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || raw.split('?').next() != Some(expected)
    {
        return Err(conflicting());
    }
    let mut map = BTreeMap::new();
    for (key, value) in url.query_pairs() {
        if map.len() >= 64 {
            return Err(conflicting());
        }
        if let Some(old) = map.insert(key.into_owned(), value.to_string()) {
            if old.as_str() != value.as_ref() {
                return Err(conflicting());
            }
        }
    }
    let denied = !unique(&map, &["error", "error_code", "err", "errorCode"])?.is_empty()
        || unique(&map, &["isRedirect"])? == "false";
    if denied {
        return Ok(Callback {
            code: String::new(),
            login_host: String::new(),
            profile: Value::Null,
            denied: true,
        });
    }
    if !unique(
        &map,
        &[
            "refreshToken",
            "refresh_token",
            "RefreshToken",
            "refresh-token",
        ],
    )?
    .is_empty()
    {
        return Err(Error::http(
            400,
            "Trae 返回了无法验证PKCE的续期凭据；请在已登录管理页使用JSON导入，不自动接收此回调",
        )
        .with_code("callback_refresh_import_required"));
    }
    let direct = unique(
        &map,
        &[
            "authCode",
            "auth_code",
            "AuthCode",
            "authorization_code",
            "code",
        ],
    )?;
    let mut code = (!direct.is_empty()).then_some(direct);
    for key in ["authCodeInfo", "auth_code_info", "AuthCodeInfo"] {
        if let Some(raw) = map.get(key).filter(|v| !v.is_empty()) {
            let value: Value = serde_json::from_str(raw).map_err(|_| conflicting())?;
            nested_code(&value, 0, &mut code)?;
        }
    }
    let code = code
        .filter(|v| {
            !v.is_empty()
                && v.len() <= 8192
                && v.trim() == v.as_str()
                && !v.chars().any(char::is_control)
        })
        .ok_or_else(|| Error::http(400, "Trae 回调缺少唯一有效授权码"))?;
    let host = unique(
        &map,
        &[
            "loginHost",
            "login_host",
            "LoginHost",
            "host",
            "consoleHost",
        ],
    )?;
    let login_host = if host.is_empty() {
        String::new()
    } else {
        util::account_origin(&host)?
    };
    let raw_profile = unique(&map, &["userInfo", "user_info", "UserInfo", "userinfo"])?;
    Ok(Callback {
        code,
        login_host,
        profile: profile_json(&raw_profile),
        denied: false,
    })
}
