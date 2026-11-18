use std::collections::BTreeMap;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use url::Url;

use crate::{endpoints, util};

const MAX_COOKIE_BYTES: usize = 48 * 1024;

pub struct Cookies {
    pub header: String,
    pub values: BTreeMap<String, String>,
    pub origin: String,
    pub rows: Option<Vec<Value>>,
}

fn invalid() -> Error {
    Error::http(
        400,
        "请提供同一个百度搭子账号的完整 Cookie 或单账号 JSON；百度云登录需要完整 BCE 会话",
    )
}

fn domain_ok(domain: &str) -> bool {
    ["dumate.cn", "baidu.com"]
        .iter()
        .any(|root| domain == *root || domain.ends_with(&format!(".{root}")))
}

fn pair(values: &mut BTreeMap<String, String>, name: &str, value: &str) -> Result<()> {
    let name = name.trim();
    let value = value.trim();
    let value = if value.len() >= 2
        && (value.starts_with('"') && value.ends_with('"')
            || value.starts_with('\'') && value.ends_with('\''))
    {
        &value[1..value.len() - 1]
    } else {
        value
    };
    if name.is_empty()
        || name.len() > 128
        || value.len() > 16 * 1024
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b';')
    {
        return Err(invalid());
    }
    if values.get(name).is_some_and(|old| old != value) {
        return Err(Error::http(
            400,
            "Cookie 中同名凭据不一致，请只导入一个百度搭子账号",
        ));
    }
    values.insert(name.to_string(), value.to_string());
    Ok(())
}

fn origin(input: &Value) -> Result<String> {
    if let Some(origin) = input.get("webOrigin").filter(|value| !value.is_null()) {
        let origin = origin.as_str().ok_or_else(invalid)?.trim_end_matches('/');
        if ![endpoints::WEB, endpoints::CONSOLE].contains(&origin) {
            return Err(invalid());
        }
        return Ok(origin.to_string());
    }
    // 原有持久 Cookie 快照没有来源字段，保持原 www 端点，不让推断迁走合法旧账号。
    if input["schema"] == 1 && input.get("validatedAt").is_some() {
        return Ok(endpoints::WEB.to_string());
    }
    let cookie = input
        .as_array()
        .map(|_| input)
        .or_else(|| input.get("cookies"))
        .or_else(|| input.get("cookie"));
    let console = input.get("bce-long-term-sessionid").is_some()
        || input
            .get("profile")
            .is_some_and(|profile| profile.get("bceUserId").is_some())
        || cookie.and_then(Value::as_array).is_some_and(|rows| {
            rows.iter()
                .any(|row| row["name"] == "bce-long-term-sessionid")
        })
        || cookie
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("bce-long-term-sessionid="));
    Ok(if console {
        endpoints::CONSOLE
    } else {
        endpoints::WEB
    }
    .to_string())
}

fn read(
    value: &Value,
    values: &mut BTreeMap<String, String>,
    rows: &mut Option<Vec<Value>>,
    origin: &str,
    depth: usize,
) -> Result<()> {
    if depth > 8 {
        return Err(invalid());
    }
    if let Some(text) = value.as_str() {
        let text = text.trim();
        if text.starts_with('{') || text.starts_with('[') {
            let parsed = serde_json::from_str(text).map_err(|_| invalid())?;
            return read(&parsed, values, rows, origin, depth + 1);
        }
        let text = if text
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("cookie:"))
        {
            text[7..].trim()
        } else {
            text
        };
        for segment in text
            .split([';', '\r', '\n'])
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            let (name, value) = segment.split_once('=').ok_or_else(invalid)?;
            pair(values, name, value)?;
        }
    } else if let Some(list) = value.as_array() {
        if list.is_empty() || list.len() > 256 {
            return Err(invalid());
        }
        let url = Url::parse(origin).map_err(|_| invalid())?;
        let host = url.host_str().ok_or_else(invalid)?;
        let mut selected: BTreeMap<String, (usize, String)> = BTreeMap::new();
        let mut projected = Vec::new();
        for row in list {
            let name = row["name"].as_str().ok_or_else(invalid)?;
            let value = row["value"].as_str().ok_or_else(invalid)?;
            if value.is_empty() {
                continue;
            }
            let domain = match row.get("domain").filter(|domain| !domain.is_null()) {
                Some(domain) => domain
                    .as_str()
                    .ok_or_else(invalid)?
                    .trim_start_matches('.')
                    .to_ascii_lowercase(),
                None => host.to_string(),
            };
            if !domain_ok(&domain)
                || !(host == domain
                    || row["hostOnly"] != true && host.ends_with(&format!(".{domain}")))
            {
                continue;
            }
            let path = row.get("path").and_then(Value::as_str).unwrap_or("/");
            if !("/api/dumate/".starts_with(path) || path == "/api/dumate") {
                continue;
            }
            let expiry = row
                .get("expirationDate")
                .or_else(|| row.get("expires"))
                .filter(|at| !at.is_null());
            if expiry
                .and_then(util::timestamp)
                .is_some_and(|at| at <= sdk::now())
            {
                continue;
            }
            let mut checked = BTreeMap::new();
            pair(&mut checked, name, value)?;
            let value = checked.remove(name).ok_or_else(invalid)?;
            let priority = domain.len() + path.len();
            match selected.get(name) {
                Some((old, previous)) if *old == priority && previous != &value => {
                    return Err(invalid())
                }
                Some((old, _)) if *old > priority => continue,
                _ => {
                    selected.insert(name.to_string(), (priority, value.clone()));
                }
            }
            let mut projected_row = json!({"name":name,"value":value,"domain":domain,"path":path});
            if let Some(at) = expiry {
                projected_row["expirationDate"] = at.clone();
            }
            projected.push(projected_row);
        }
        for (name, (_, value)) in selected {
            pair(values, &name, &value)?;
        }
        *rows = Some(projected);
    } else if let Some(fields) = value.as_object() {
        if fields.contains_key("accounts") {
            return Err(Error::http(
                400,
                "请从导出文件中选择单个账号，不要导入整个账号池",
            ));
        }
        if let Some(cookie) = ["cookies", "cookie", "cookieHeader"]
            .iter()
            .filter_map(|key| fields.get(*key))
            .find(|value| !value.is_null() && value.as_str() != Some(""))
        {
            return read(cookie, values, rows, origin, depth + 1);
        }
        for (name, value) in fields {
            pair(values, name, value.as_str().ok_or_else(invalid)?)?;
        }
    } else {
        return Err(invalid());
    }
    Ok(())
}

impl Cookies {
    pub fn parse(input: &Value) -> Result<Self> {
        if input.to_string().len() > 256 * 1024 {
            return Err(invalid());
        }
        let input = payload(input)?;
        let origin = origin(&input)?;
        let mut values = BTreeMap::new();
        let mut rows = None;
        read(&input, &mut values, &mut rows, &origin, 0)?;
        if origin == endpoints::CONSOLE {
            if !["bce-long-term-sessionid", "bce-user-info"]
                .iter()
                .all(|name| values.get(*name).is_some_and(|value| !value.is_empty()))
            {
                return Err(invalid());
            }
        } else if !["BDUSS", "BDUSS_BFESS"]
            .iter()
            .any(|name| values.get(*name).is_some_and(|value| value.len() >= 8))
        {
            return Err(invalid());
        }
        let header = values
            .iter()
            .map(|(name, value)| {
                let value = if value.bytes().any(|byte| b" :|=\t".contains(&byte)) {
                    format!("\"{value}\"")
                } else {
                    value.clone()
                };
                format!("{name}={value}")
            })
            .collect::<Vec<_>>()
            .join("; ");
        if header.len() > MAX_COOKIE_BYTES {
            return Err(invalid());
        }
        Ok(Self {
            header,
            values,
            origin,
            rows,
        })
    }

    pub fn protected(&self, secret: &Value) -> Vec<String> {
        let mut protected = self.values.values().cloned().collect::<Vec<_>>();
        protected.push(self.header.clone());
        let token = util::text(secret, &["appToken"]);
        if !token.is_empty() {
            protected.push(token);
        }
        protected
    }

    pub fn tail(&self) -> String {
        ["BDUSS", "BDUSS_BFESS", "bce-long-term-sessionid"]
            .iter()
            .find_map(|name| self.values.get(*name))
            .map(|value| {
                value
                    .chars()
                    .rev()
                    .take(6)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect()
            })
            .unwrap_or_default()
    }
}

pub fn payload(value: &Value) -> Result<Value> {
    payload_at(value, 0)
}

fn payload_at(value: &Value, depth: usize) -> Result<Value> {
    if depth > 8 {
        return Err(invalid());
    }
    if let Some(text) = value.as_str() {
        let text = text.trim();
        if text.starts_with('{') || text.starts_with('[') {
            let parsed = serde_json::from_str(text).map_err(|_| invalid())?;
            return payload_at(&parsed, depth + 1);
        }
        return Ok(json!({"cookie":text}));
    }
    if let Some(fields) = value.as_object() {
        if fields.contains_key("accounts") {
            return Err(invalid());
        }
        for key in ["cookies", "cookie", "cookieHeader"] {
            if let Some(raw) = fields
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|raw| raw.starts_with('{') || raw.starts_with('['))
            {
                let parsed = serde_json::from_str(raw).map_err(|_| invalid())?;
                let mut normalized = value.clone();
                normalized[key] = parsed;
                return payload_at(&normalized, depth + 1);
            }
        }
        if !["cookie", "cookies", "cookieHeader"]
            .iter()
            .any(|key| fields.get(*key).is_some())
        {
            if let Some(nested) = ["secret", "credential", "credentials"]
                .iter()
                .filter_map(|key| fields.get(*key))
                .find(|value| value.is_object())
            {
                return payload_at(nested, depth + 1);
            }
        }
    }
    Ok(value.clone())
}

pub fn input(params: &Value) -> Result<(Value, Option<String>)> {
    let source = &params["source"];
    let value = match source["kind"].as_str() {
        Some("file_text") => payload(&source["content"])?,
        Some("manual") => {
            let value = &source["payload"];
            if let Some(index) = value.get("methodIndex") {
                let index = index
                    .as_u64()
                    .filter(|index| *index <= 1)
                    .ok_or_else(invalid)?;
                let fields = value["inputs"].as_object().ok_or_else(invalid)?;
                let key = if index == 0 {
                    "cookie"
                } else {
                    "credentialJson"
                };
                if fields.iter().any(|(name, value)| {
                    ![key, "name"].contains(&name.as_str()) || !value.is_string()
                }) {
                    return Err(invalid());
                }
                let input = payload(fields.get(key).ok_or_else(invalid)?)?;
                let name = fields
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string);
                return Ok((input, name));
            }
            payload(value)?
        }
        Some("desktop") => {
            let read = sdk::host(
                "native.call",
                json!({"helper":"desktop.credentials.dumate.v1","operation":"read","payload":{}}),
            )?;
            payload(&read["content"])?
        }
        Some("file_base64") => {
            return Err(Error::http(
                400,
                "百度搭子只接受文本凭据 JSON，不导入 SQLite 或其他二进制文件",
            ))
        }
        _ => return Err(invalid()),
    };
    Ok((value, None))
}
