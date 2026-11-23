//! Cookie头/编辑器JSON/嵌套对象共用解析，保留同一会话extras与原BDUSS身份。
//! 只解析用户显式传入文本；不扫Cookie库或系统浏览器。同名不同值拒绝，避免拼接账号。

use crate::util;
use agent2api_wasm_sdk::{Error, Result, Value};

const MAX_COOKIE: usize = 64 * 1024;
#[derive(Clone)]
pub struct Credentials {
    pub pairs: Vec<(String, String)>,
    pub uid: String,
    pub nickname: String,
}
fn bad(message: &str) -> Error {
    Error::http(400, message).with_code("kuku_cookie_invalid")
}
fn pair(out: &mut Vec<(String, String)>, name: &str, value: &str) -> Result<()> {
    let name = name.trim();
    let value = value.trim();
    if name.is_empty() || value.is_empty() {
        return Ok(());
    }
    if name.len() > 256
        || !name
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&ch))
        || value.len() > 8192
        || !value
            .bytes()
            .all(|ch| (0x21..=0x7e).contains(&ch) && !b";\r\n".contains(&ch))
    {
        return Err(bad("库库Cookie名称或内容无效"));
    }
    if name.eq_ignore_ascii_case("gfprotpl") {
        return Ok(());
    }
    if let Some((_, old)) = out.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)) {
        if old != value {
            return Err(bad("Cookie包含同名不同值，请仅导出一个库库登录会话"));
        }
        return Ok(());
    }
    if out.len() >= 128 {
        return Err(bad("库库Cookie数量过多"));
    }
    let name = if name.eq_ignore_ascii_case("BDUSS") {
        "BDUSS"
    } else if name.eq_ignore_ascii_case("STOKEN") {
        "STOKEN"
    } else {
        name
    };
    out.push((name.into(), value.into()));
    Ok(())
}
fn cookie_text(text: &str, out: &mut Vec<(String, String)>, depth: usize) -> Result<()> {
    if text.len() > MAX_COOKIE {
        return Err(bad("库库Cookie文本过大"));
    }
    let text = text.trim();
    if text.starts_with('{') || text.starts_with('[') {
        let value: Value =
            serde_json::from_str(text).map_err(|_| bad("库库Cookie JSON格式无效"))?;
        return parse(&value, out, depth + 1);
    }
    let text = text
        .get(..7)
        .filter(|prefix| prefix.eq_ignore_ascii_case("cookie:"))
        .map(|_| &text[7..])
        .unwrap_or(text)
        .trim();
    if text.chars().any(char::is_control) {
        return Err(bad("库库Cookie不能含换行或控制字符"));
    }
    for segment in text
        .split(';')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
    {
        let (name, value) = segment
            .split_once('=')
            .ok_or_else(|| bad("库库Cookie须为name=value字段"))?;
        pair(out, name, value)?;
    }
    Ok(())
}
fn baidu_domain(domain: &str) -> bool {
    let domain = domain
        .trim_start_matches('.')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    domain == "baidu.com" || domain.ends_with(".baidu.com")
}
fn parse(value: &Value, out: &mut Vec<(String, String)>, depth: usize) -> Result<()> {
    if depth > 8 {
        return Err(bad("库库凭据嵌套过深"));
    }
    match value {
        Value::String(text) => cookie_text(text, out, depth),
        Value::Array(items) => {
            if items.len() > 256 {
                return Err(bad("Cookie编辑器条目过多"));
            }
            for item in items {
                if item
                    .get("domain")
                    .and_then(Value::as_str)
                    .is_some_and(|domain| !baidu_domain(domain))
                {
                    continue;
                }
                let name = item["name"]
                    .as_str()
                    .ok_or_else(|| bad("Cookie编辑器条目缺少name"))?;
                let value = item["value"]
                    .as_str()
                    .ok_or_else(|| bad("Cookie编辑器条目缺少value"))?;
                pair(out, name, value)?;
            }
            Ok(())
        }
        Value::Object(map) => {
            if map
                .get("provider")
                .and_then(Value::as_str)
                .is_some_and(|provider| provider != "kuku")
            {
                return Err(bad("此凭据不属于库库提供商"));
            }
            let has_direct = map.keys().any(|key| {
                ["BDUSS", "BDUSS_BFESS", "STOKEN"]
                    .iter()
                    .any(|name| key.eq_ignore_ascii_case(name))
            });
            if has_direct {
                for (key, value) in map {
                    if [
                        "uid",
                        "name",
                        "loginName",
                        "source",
                        "provider",
                        "tokenTail",
                        "identityTail",
                        "desktop",
                    ]
                    .contains(&key.as_str())
                    {
                        continue;
                    }
                    if let Some(value) = value.as_str() {
                        pair(out, key, value)?;
                    }
                }
                return Ok(());
            }
            for key in [
                "accessToken",
                "auth",
                "cookie",
                "cookies",
                "credentials",
                "secret",
            ] {
                if let Some(inner) = map.get(key).filter(|value| {
                    !value.is_null() && !value.as_str().is_some_and(|text| text.trim().is_empty())
                }) {
                    return parse(inner, out, depth + 1);
                }
            }
            Err(bad("库库凭据中未找到Cookie字段"))
        }
        _ => Err(bad("库库凭据须为Cookie文本、对象或编辑器数组")),
    }
}
impl Credentials {
    pub fn from_value(value: &Value) -> Result<Self> {
        if value.to_string().len() > 128 * 1024 {
            return Err(bad("库库凭据内容过大"));
        }
        let mut pairs = Vec::new();
        parse(value, &mut pairs, 0)?;
        if !pairs.iter().any(|(name, _)| name == "BDUSS") {
            if let Some((_, value)) = pairs
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("BDUSS_BFESS"))
            {
                let value = value.clone();
                pair(&mut pairs, "BDUSS", &value)?;
            }
        }
        let c = Self {
            pairs,
            uid: String::new(),
            nickname: String::new(),
        };
        if c.get("BDUSS").len() < 8 {
            return Err(bad("缺少有效BDUSS，请粘贴完整库库Cookie"));
        }
        if c.cookie().len() > MAX_COOKIE {
            return Err(bad("库库Cookie总长度超过上限"));
        }
        Ok(c)
    }
    pub fn from_secret(secret: &Value) -> Result<Self> {
        let mut c = Self::from_value(secret)?;
        c.uid = util::text(secret, &["uid"]);
        c.nickname = util::text(secret, &["loginName"]);
        if c.uid.len() > 480
            || c.uid.chars().any(char::is_control)
            || c.secrets()
                .iter()
                .filter(|secret| !secret.is_empty())
                .any(|secret| c.uid.contains(*secret))
        {
            return Err(bad("库库账号标识无效"));
        }
        Ok(c)
    }
    pub fn get(&self, name: &str) -> &str {
        self.pairs
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .unwrap_or("")
    }
    pub fn ptoken(&self) -> &str {
        let primary = self.get("PTOKEN");
        if primary.is_empty() {
            self.get("PTOKEN_BFESS")
        } else {
            primary
        }
    }
    pub fn set_stoken(&mut self, value: &str) -> Result<()> {
        self.pairs
            .retain(|(key, _)| !key.eq_ignore_ascii_case("STOKEN"));
        pair(&mut self.pairs, "STOKEN", value)
    }
    pub fn cookie(&self) -> String {
        let mut parts: Vec<_> = self
            .pairs
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        parts.push("gfprotpl=genflowpro".into());
        parts.join("; ")
    }
    pub fn binding(&self) -> String {
        util::hash(&self.cookie())
    }
    pub fn secrets(&self) -> Vec<&str> {
        self.pairs
            .iter()
            .filter(|(name, value)| {
                value.len() >= 8
                    || ["BDUSS", "STOKEN", "PTOKEN", "PTOKEN_BFESS"]
                        .iter()
                        .any(|key| name.eq_ignore_ascii_case(key))
            })
            .map(|(_, value)| value.as_str())
            .collect()
    }
    pub fn original_tail(&self) -> String {
        let bduss = self.get("BDUSS");
        if bduss.len() <= 8 {
            String::new()
        } else {
            bduss
                .chars()
                .rev()
                .take(8)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect()
        }
    }
}
