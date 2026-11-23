//! 原生 Cookie 协议的严格归一化；不读文件、环境变量或浏览器。
//! 手工 Cookie 头没有到期属性，绝不推算 addedAt + 30 天。旧 expiresAt
//! 可能是原生估算值，本批不把它升级为可信期限，也不预先接受尚未定义的宿主标记。

use agent2api_wasm_sdk::{Error, Result, Value};

use crate::endpoints;

const MAX_INPUT: usize = 64 * 1024;
const MAX_SESSION: usize = 8192;

fn unquote(text: &str) -> &str {
    let text = text.trim();
    if text.len() >= 2
        && ((text.starts_with('"') && text.ends_with('"'))
            || (text.starts_with('\'') && text.ends_with('\'')))
    {
        text.get(1..text.len() - 1).unwrap_or("").trim()
    } else {
        text
    }
}

fn invalid() -> Error {
    Error::http(
        400,
        "请粘贴 MonkeyCode 国内版的 Session 或完整 Cookie；不接受多个不同 Session、空白或控制字符",
    )
    .with_code("monkeycode_invalid_session")
}

fn validate(value: &str) -> Result<String> {
    if value.is_empty()
        || value.len() > MAX_SESSION
        || !value.bytes().all(
            |byte| matches!(byte, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e),
        )
    {
        return Err(invalid());
    }
    Ok(value.to_string())
}

pub fn normalize(raw: &str) -> Result<String> {
    if raw.len() > MAX_INPUT || raw.chars().any(char::is_control) {
        return Err(invalid());
    }
    let mut text = unquote(raw);
    let has_header = text
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("cookie:"));
    if has_header {
        text = unquote(text.get(7..).unwrap_or(""));
    }
    let mut found: Option<String> = None;
    for part in text.split(';') {
        let Some((name, value)) = part.trim().split_once('=') else {
            continue;
        };
        if name.trim() != endpoints::SESSION_COOKIE {
            continue;
        }
        let value = validate(unquote(value))?;
        if found.as_ref().is_some_and(|previous| previous != &value) {
            return Err(invalid());
        }
        found = Some(value);
    }
    if let Some(value) = found {
        return Ok(value);
    }
    // 与原生一致：含等号却没有本家 Cookie 名，不能猜成某个会话值。
    if has_header || text.contains([';', '=']) {
        return Err(invalid());
    }
    validate(unquote(text))
}

#[derive(Default)]
pub struct Input {
    pub session: String,
    pub user_id: Option<String>,
    pub image_id: Option<String>,
    pub name: Option<String>,
}

#[derive(Default)]
struct Collector {
    session: Option<String>,
    user_id: Option<String>,
    image_id: Option<String>,
    name: Option<String>,
    remaining: usize,
}

fn consensus(target: &mut Option<String>, value: String) -> Result<()> {
    if target.as_ref().is_some_and(|previous| previous != &value) {
        return Err(invalid());
    }
    *target = Some(value);
    Ok(())
}

fn optional_text(raw: &Value, limit: usize) -> Result<Option<String>> {
    match raw {
        Value::Null => Ok(None),
        Value::String(text) if text.trim().is_empty() => Ok(None),
        Value::String(text) if text.len() <= limit && !text.chars().any(char::is_control) => {
            Ok(Some(text.trim().to_string()))
        }
        _ => Err(invalid()),
    }
}

impl Collector {
    fn visit(&mut self, value: &Value, depth: usize, user: bool) -> Result<()> {
        if depth > 8 || self.remaining == 0 {
            return Err(invalid());
        }
        self.remaining -= 1;
        if let Some(raw) = value.as_str() {
            let text = raw.trim();
            if text.len() > MAX_INPUT {
                return Err(invalid());
            }
            if text.starts_with('{') {
                let parsed: Value = serde_json::from_str(text).map_err(|_| invalid())?;
                return self.visit(&parsed, depth + 1, user);
            }
            return consensus(&mut self.session, normalize(raw)?);
        }
        let fields = value.as_object().ok_or_else(invalid)?;
        if fields.len() > 128 {
            return Err(invalid());
        }
        if fields
            .get("methodIndex")
            .is_some_and(|index| !index.is_null() && !matches!(index.as_u64(), Some(0 | 2)))
        {
            return Err(crate::gates::web_login());
        }
        if fields
            .get("desktop")
            .is_some_and(|value| !value.is_null() && value != false)
            || fields
                .get("provider")
                .is_some_and(|value| !value.is_null() && value != endpoints::ID)
        {
            return Err(Error::http(
                403,
                "MonkeyCode 导入只接受国内手工凭据，不从 JSON 授予本机或其它地区权限",
            ));
        }
        for key in [
            "accessToken",
            "session",
            "token",
            "cookie",
            "Cookie",
            "monkeycode_ai_session",
        ] {
            if let Some(raw) = fields.get(key) {
                if let Some(text) = optional_text(raw, MAX_INPUT)? {
                    consensus(&mut self.session, normalize(&text)?)?;
                }
            }
        }
        for key in ["userId", "user_id"] {
            if let Some(raw) = fields.get(key) {
                if let Some(id) = optional_text(raw, 128)? {
                    consensus(&mut self.user_id, id)?;
                }
            }
        }
        if user {
            if let Some(raw) = fields.get("id") {
                if let Some(id) = optional_text(raw, 128)? {
                    consensus(&mut self.user_id, id)?;
                }
            }
        }
        for key in ["imageId", "image_id"] {
            if let Some(raw) = fields.get(key) {
                if let Some(image) = optional_text(raw, 128)? {
                    consensus(&mut self.image_id, image)?;
                }
            }
        }
        if self.name.is_none() {
            if let Some(raw) = fields.get("name") {
                self.name = optional_text(raw, 256)?;
            }
        }
        for key in [
            "inputs",
            "credentialJson",
            "credentials",
            "secret",
            "pluginSecret",
            "payload",
            "data",
            "user",
        ] {
            if let Some(value) = fields
                .get(key)
                .filter(|value| !value.is_null() && value.as_str() != Some(""))
            {
                self.visit(value, depth + 1, key == "user")?;
            }
        }
        Ok(())
    }
}

/// 显式表单、完整账号 JSON 和登录窗口凭据均归一到同一值；别名冲突不任选一项。
pub fn input(payload: &Value) -> Result<Input> {
    if payload.to_string().len() > MAX_INPUT {
        return Err(invalid());
    }
    let mut input = Collector {
        remaining: 64,
        ..Default::default()
    };
    input.visit(payload, 0, false)?;
    Ok(Input {
        session: input.session.ok_or_else(|| {
            Error::http(400, "请提供 MonkeyCode 国内 Session、Cookie 或完整 JSON")
        })?,
        user_id: input.user_id,
        image_id: input.image_id,
        name: input.name,
    })
}

pub fn session(secret: &Value) -> Result<String> {
    let mut found = None;
    for key in ["accessToken", "session", "token", "cookie", "Cookie"] {
        let Some(raw) = secret.get(key).filter(|value| !value.is_null()) else {
            continue;
        };
        let raw = raw.as_str().ok_or_else(invalid)?.trim();
        if raw.is_empty() {
            continue;
        }
        let value = normalize(raw)?;
        if found.as_ref().is_some_and(|previous| previous != &value) {
            return Err(invalid());
        }
        found = Some(value);
    }
    found.ok_or_else(|| {
        Error::http(401, "MonkeyCode 账号缺少登录态，请重新粘贴 Session")
            .with_code("monkeycode_session_missing")
    })
}

pub fn valid_image_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
        && value
            .bytes()
            .any(|byte| matches!(byte, b'1'..=b'9' | b'a'..=b'f' | b'A'..=b'F'))
}

pub fn tail(session: &str) -> String {
    // 短值不展示完整凭证，正常值也最多六个字符。
    if session.chars().count() <= 6 {
        return String::new();
    }
    let start = session.chars().count().saturating_sub(6);
    session.chars().skip(start).collect()
}

pub fn expiry(_secret: &Value) -> Option<i64> {
    // 可信浏览器事件合同未确定前，没有可证明的 Cookie 实际期限。
    None
}
