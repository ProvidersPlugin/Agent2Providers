//! Session 是不透明值，不解析 JWT/期限。所有别名、嵌套手动 JSON 和同名 Cookie
//! 必须归一后相同；不读文件、浏览器、环境变量，也不透传整份 Cookie。

use crate::{endpoints, provenance, redact::Guard, strict_json, util};
use agent2api_wasm_sdk::{Error, Result, Value};

const MAX_INPUT: usize = 64 * 1024;
const MAX_SESSION: usize = 8192;
const SESSION_KEYS: &[&str] = &[
    "accessToken",
    "session",
    "token",
    "cookie",
    "Cookie",
    "cookies",
    "monkeycode_ai_session",
];
pub const USER_KEYS: &[&str] = &["userId", "user_id", "userid", "uid"];

pub struct Input {
    pub session: String,
    pub user_id: Option<String>,
    pub identity: Option<String>,
    pub record_id: Option<String>,
    pub name: Option<String>,
    pub email: Option<String>,
    pub image_id: Option<String>,
    pub guard: Guard,
}

#[derive(Default)]
struct Collector {
    session: Option<String>,
    user_id: Option<String>,
    identity: Option<String>,
    record_id: Option<String>,
    name: Option<String>,
    email: Option<String>,
    image_id: Option<String>,
    guard: Guard,
    candidate_count: usize,
}

fn invalid() -> Error {
    Error::http(
        400,
        "请提供国际站 Session、完整 Cookie 或手动 JSON；字段无效、冲突或超限时不会导入",
    )
    .with_code("monkeycode_intl_invalid_session")
}

fn unquote(value: &str) -> &str {
    let text = value.trim();
    if text.len() >= 2
        && ((text.starts_with('"') && text.ends_with('"'))
            || (text.starts_with('\'') && text.ends_with('\'')))
    {
        text.get(1..text.len() - 1).unwrap_or("").trim()
    } else {
        text
    }
}

fn valid_session(value: &str) -> Result<String> {
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
    let header = text
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("cookie:"));
    if header {
        text = unquote(text.get(7..).unwrap_or(""));
    }
    let mut session = None;
    for part in text.split(';') {
        let Some((name, value)) = part.trim().split_once('=') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("domain") {
            provenance::declaration("domain", &Value::String(unquote(value).to_string()))?;
        }
        if name.trim() == endpoints::SESSION_COOKIE {
            util::consensus(&mut session, valid_session(unquote(value))?)?;
        }
    }
    if let Some(session) = session {
        return Ok(session);
    }
    if header || text.contains([';', '=']) {
        return Err(invalid());
    }
    valid_session(text)
}

fn id_value(value: &str) -> Result<String> {
    if !util::uuid(value) {
        return Err(Error::http(400, "MonkeyCode 国际用户身份必须是有效 UUID"));
    }
    Ok(value.to_ascii_lowercase())
}

fn short(value: &str, limit: usize) -> Result<String> {
    if value.len() > limit || value.chars().any(char::is_control) {
        return Err(invalid());
    }
    Ok(value.to_string())
}

impl Collector {
    fn raw(&mut self, raw: &str, depth: usize) -> Result<()> {
        if raw.len() > MAX_INPUT || depth > 8 {
            return Err(invalid());
        }
        self.candidate_count = self.candidate_count.saturating_add(1);
        if self.candidate_count > 64 {
            return Err(invalid());
        }
        let raw = raw.trim();
        // JSON 可含格式化换行，解析后逐字段验证；不能把它作为 Cookie 直接发出。
        if raw.starts_with(['{', '[', '"']) {
            if let Some(value) = strict_json::parse(raw.as_bytes()) {
                provenance::nested(&value, 0)?;
                return self.visit(&value, depth + 1);
            }
            if raw.starts_with(['{', '[']) {
                return Err(invalid());
            }
        }
        let session = normalize(raw)?;
        self.guard.cookie(raw)?;
        self.guard.add(&session)?;
        util::consensus(&mut self.session, session)
    }

    fn visit(&mut self, value: &Value, depth: usize) -> Result<()> {
        if depth > 8 {
            return Err(invalid());
        }
        if let Some(raw) = value.as_str() {
            return self.raw(raw, depth);
        }
        let fields = value.as_object().ok_or_else(invalid)?;
        provenance::check(value)?;
        self.guard.observe(value)?;
        if let Some(index) = fields.get("methodIndex").filter(|index| !index.is_null()) {
            if !matches!(index.as_u64(), Some(0 | 2)) {
                return Err(crate::gates::web_login());
            }
        }
        for key in SESSION_KEYS {
            if let Some(raw) = fields.get(*key) {
                match raw {
                    Value::Null => {}
                    Value::String(text) if text.trim().is_empty() => {}
                    Value::String(text) => {
                        let is_cookie = ["cookie", "Cookie", "cookies"].contains(key);
                        if is_cookie
                            && !text.trim_start().starts_with(['{', '"'])
                            && !text
                                .trim_start()
                                .get(..7)
                                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("cookie:"))
                        {
                            self.raw(&format!("Cookie: {text}"), depth + 1)?;
                        } else {
                            self.raw(text, depth + 1)?;
                        }
                    }
                    Value::Object(_) => self.visit(raw, depth + 1)?,
                    _ => return Err(invalid()),
                }
            }
        }
        for key in USER_KEYS {
            if let Some(raw) = fields.get(*key) {
                if let Some(text) = util::optional_text(raw)? {
                    util::consensus(&mut self.user_id, id_value(text)?)?;
                }
            }
        }
        for key in ["identity", "pluginIdentity", "expectedIdentity"] {
            if let Some(raw) = fields.get(key) {
                if let Some(text) = util::optional_text(raw)? {
                    util::consensus(&mut self.identity, short(text, 512)?)?;
                }
            }
        }
        for key in ["id", "recordId", "accountId"] {
            if let Some(raw) = fields.get(key) {
                if let Some(text) = util::optional_text(raw)? {
                    util::consensus(&mut self.record_id, short(text, 512)?)?;
                }
            }
        }
        for key in ["imageId", "image_id"] {
            if let Some(raw) = fields.get(key) {
                if let Some(text) = util::optional_text(raw)? {
                    util::consensus(&mut self.image_id, short(text, 128)?)?;
                }
            }
        }
        // 备注名/邮箱不是身份别名，允许外层显式备注覆盖内层用户展示名。
        for (key, slot, max) in [
            ("name", &mut self.name, 256),
            ("email", &mut self.email, 320),
        ] {
            if let Some(raw) = fields.get(key) {
                if let Some(text) = util::optional_text(raw)? {
                    let text = short(text, max)?;
                    if slot.is_none() {
                        *slot = Some(text);
                    }
                }
            }
        }
        if let Some(user) = fields.get("user") {
            let user_fields = user.as_object().ok_or_else(invalid)?;
            provenance::check(user)?;
            if let Some(raw) = user_fields.get("id") {
                if let Some(text) = util::optional_text(raw)? {
                    util::consensus(&mut self.user_id, id_value(text)?)?;
                }
            }
            let mut user = user.clone();
            if let Some(fields) = user.as_object_mut() {
                fields.remove("id");
            }
            self.visit(&user, depth + 1)?;
        }
        for key in [
            "inputs",
            "credentialJson",
            "payload",
            "secret",
            "pluginSecret",
            "credentials",
            "data",
            "public",
            "pluginPublic",
        ] {
            if let Some(nested) = fields.get(key) {
                if !nested.is_null() {
                    self.visit(nested, depth + 1)?;
                }
            }
        }
        if let Some(headers) = fields.get("headers") {
            let headers = headers.as_object().ok_or_else(invalid)?;
            for (key, raw) in headers {
                if key.eq_ignore_ascii_case("cookie") {
                    if let Some(text) = util::optional_text(raw)? {
                        self.raw(&format!("Cookie: {text}"), depth + 1)?;
                    }
                } else if ["origin", "host"]
                    .iter()
                    .any(|name| key.eq_ignore_ascii_case(name))
                {
                    provenance::check(&serde_json::json!({ key.to_ascii_lowercase(): raw }))?;
                }
            }
        }
        Ok(())
    }
}

pub fn parse(value: &Value) -> Result<Input> {
    provenance::nested(value, 0)?;
    parse_many(&[value])
}

pub fn parse_many(values: &[&Value]) -> Result<Input> {
    if values
        .iter()
        .map(|value| value.to_string().len())
        .sum::<usize>()
        > MAX_INPUT
    {
        return Err(invalid());
    }
    let mut collected = Collector::default();
    for value in values {
        collected.visit(value, 0)?;
    }
    let session = collected.session.ok_or_else(invalid)?;
    if collected
        .user_id
        .as_ref()
        .is_some_and(|id| collected.guard.contains(id))
        || collected
            .image_id
            .as_ref()
            .is_some_and(|id| collected.guard.contains(id))
    {
        return Err(invalid());
    }
    Ok(Input {
        session,
        user_id: collected.user_id,
        identity: collected.identity,
        record_id: collected.record_id,
        name: collected.name,
        email: collected.email,
        image_id: collected.image_id,
        guard: collected.guard,
    })
}

pub fn tail(session: &str) -> String {
    if session.len() <= 6 {
        String::new()
    } else {
        session
            .chars()
            .skip(session.chars().count().saturating_sub(6))
            .collect()
    }
}
