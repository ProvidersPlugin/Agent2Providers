//! Cookie 快照解析，不使用 Cookie jar、本机文件、CDP 或环境变量。
//! 与原生输入别名兼容；冲突 session 拒绝而不随机选一个，公开出口不返回此结构。

use std::collections::BTreeMap;

use agent2api_wasm_sdk::{json, Result, Value};

use crate::{endpoints, errors, gates, util};

const MAX_INPUT: usize = 256 * 1024;
const MAX_HEADER: usize = 64 * 1024;
const MAX_COOKIES: usize = 512;
const META: &[&str] = &[
    "name",
    "userId",
    "enterpriseType",
    "email",
    "avatar",
    "deviceId",
    "iid",
    "webId",
    "fingerprint",
    "clientVersion",
    "channel",
    "userAgent",
    "chromiumVersion",
    "teaUuid",
    "webTabId",
    "source",
    "desktop",
    "edition",
    "baseUrl",
    "provider",
    "id",
    "schema",
    "identity",
    "pluginIdentity",
    "tokenTail",
    "hasCredentials",
    "validation",
    "validatedAt",
];

#[derive(Clone)]
pub struct Credentials {
    pub cookie_header: String,
    pub cookies: Value,
    pub metadata: Value,
    pub protected: Vec<String>,
}

pub fn own_provider(value: &Value) -> Result<()> {
    for field in ["provider", "edition"] {
        if value
            .get(field)
            .is_some_and(|value| !value.is_null() && value.as_str() != Some(endpoints::PROVIDER))
        {
            return Err(errors::invalid("凭据不属于豆包工作国内版，未跨提供商导入"));
        }
    }
    if value.get("baseUrl").is_some_and(|value| {
        !value.is_null()
            && value
                .as_str()
                .is_none_or(|raw| raw.trim_end_matches('/') != endpoints::BASE)
    }) {
        return Err(errors::invalid("豆包工作凭据来源必须为国内官方站点"));
    }
    Ok(())
}

/// 解开用户显式提供的单账号记录，不解析路径，不枚举账号。
pub fn payload(input: &Value) -> Result<Value> {
    if input.to_string().len() > MAX_INPUT {
        return Err(errors::invalid("豆包工作凭据文本过长"));
    }
    let mut value = input.clone();
    for _ in 0..8 {
        own_provider(&value)?;
        if let Some(raw) = value.as_str() {
            let raw = raw.trim();
            if raw.starts_with('{') || raw.starts_with('[') {
                value = serde_json::from_str(raw)
                    .map_err(|_| errors::invalid("豆包工作 Cookie JSON 无法解析"))?;
                continue;
            }
        }
        let nested = value
            .get("pluginSecret")
            .or_else(|| value.get("secret"))
            .filter(|value| !value.is_null());
        if let Some(nested) = nested {
            if !nested.is_object() {
                return Err(errors::invalid("豆包工作账号快照格式无效"));
            }
            for field in ["userId", "enterpriseType"] {
                if value.get(field).is_some_and(|value| !value.is_null())
                    && nested.get(field).is_some_and(|value| !value.is_null())
                    && util::identifier(&value[field]) != util::identifier(&nested[field])
                {
                    return Err(errors::identity());
                }
                if value.get(field).is_some_and(|value| !value.is_null())
                    && nested.get(field).is_some_and(Value::is_null)
                {
                    return Err(errors::identity());
                }
            }
            let mut next = nested.clone();
            if let Some(existing) = value.get("pluginIdentity").filter(|value| !value.is_null()) {
                if next
                    .get("identity")
                    .is_some_and(|value| !value.is_null() && value != existing)
                {
                    return Err(errors::identity());
                }
                next["identity"] = existing.clone();
            }
            for key in META.iter().copied().filter(|key| {
                !matches!(
                    *key,
                    "id" | "pluginIdentity" | "provider" | "edition" | "baseUrl"
                )
            }) {
                if next.get(key).is_none() {
                    if let Some(field) = value.get(key) {
                        next[key] = field.clone();
                    }
                }
            }
            value = next;
            continue;
        }
        if value.is_object() {
            let mut parsed = None;
            for field in ["cookies", "cookieHeader", "cookie", "token"] {
                let Some(raw) = value.get(field).and_then(Value::as_str) else {
                    continue;
                };
                let raw = raw.trim();
                if raw.starts_with('{') || raw.starts_with('[') {
                    let nested: Value = serde_json::from_str(raw)
                        .map_err(|_| errors::invalid("豆包工作 Cookie JSON 无法解析"))?;
                    let mut next = value.clone();
                    next[field] = nested.clone();
                    if nested.is_object() {
                        own_provider(&nested)?;
                        for key in META.iter().copied().filter(|key| {
                            !matches!(*key, "id" | "identity" | "provider" | "edition" | "baseUrl")
                        }) {
                            if next.get(key).is_none() {
                                if let Some(field) = nested.get(key) {
                                    next[key] = field.clone();
                                }
                            } else if matches!(key, "userId" | "enterpriseType")
                                && nested.get(key).is_some_and(|value| !value.is_null())
                                && util::identifier(&next[key]) != util::identifier(&nested[key])
                            {
                                return Err(errors::identity());
                            }
                        }
                    }
                    parsed = Some(next);
                    break;
                }
            }
            if let Some(next) = parsed {
                value = next;
                continue;
            }
        }
        if value.is_object() {
            let mut metadata = json!({});
            nested_metadata(&value, 0, &mut metadata)?;
            if let Some(fields) = metadata.as_object() {
                for (key, field) in fields {
                    if value.get(key).is_none() {
                        value[key] = field.clone();
                    }
                }
            }
        }
        return Ok(value);
    }
    Err(errors::invalid("豆包工作凭据包装层数过多"))
}

fn nested_metadata(value: &Value, depth: usize, metadata: &mut Value) -> Result<()> {
    if depth > 8 {
        return Err(errors::invalid("豆包工作 Cookie 包装层数过多"));
    }
    own_provider(value)?;
    for key in META.iter().copied() {
        if let Some(field) = value.get(key) {
            if matches!(key, "userId" | "enterpriseType")
                && metadata.get(key).is_some_and(|value| !value.is_null())
                && !field.is_null()
                && util::identifier(&metadata[key]) != util::identifier(field)
            {
                return Err(errors::identity());
            }
            if metadata.get(key).is_none() {
                metadata[key] = field.clone();
            }
        }
    }
    if let Some(child) = ["cookies", "cookieHeader", "cookie", "token"]
        .iter()
        .find_map(|key| {
            value
                .get(*key)
                .filter(|value| value.is_object() || value.is_string())
        })
    {
        if let Some(raw) = child
            .as_str()
            .map(str::trim)
            .filter(|raw| raw.starts_with('{'))
        {
            let parsed: Value = serde_json::from_str(raw)
                .map_err(|_| errors::invalid("豆包工作 Cookie JSON 无法解析"))?;
            nested_metadata(&parsed, depth + 1, metadata)?;
        } else if child.is_object() {
            nested_metadata(child, depth + 1, metadata)?;
        }
    }
    Ok(())
}

pub fn snapshot_present(value: &Value) -> bool {
    [
        "cookies",
        "cookieHeader",
        "cookie",
        "token",
        "sessionid",
        "sessionid_ss",
    ]
    .iter()
    .any(|key| value.get(*key).is_some_and(|value| !value.is_null()))
        || value.is_string()
        || value.is_array()
}

fn valid_pair(name: &str, value: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && value.len() <= 16 * 1024
        && name
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !b"=;,\"\\".contains(&byte))
        && !value.chars().any(|ch| ch.is_control() || ch == ';')
}

fn put(
    pairs: &mut BTreeMap<String, (String, Option<f64>)>,
    name: &str,
    value: &str,
    expires: Option<f64>,
) -> Result<()> {
    if !valid_pair(name, value) {
        return Err(errors::invalid("豆包工作 Cookie 名称或内容无效"));
    }
    if pairs.get(name).is_some_and(|(old, _)| old != value)
        && matches!(name, "sessionid" | "sessionid_ss")
    {
        return Err(errors::invalid(
            "豆包工作 Cookie 包含互相冲突的登录会话，请仅导入一个账号",
        ));
    }
    if pairs.len() >= MAX_COOKIES && !pairs.contains_key(name) {
        return Err(errors::invalid("豆包工作 Cookie 条数过多"));
    }
    pairs.insert(name.to_string(), (value.to_string(), expires));
    Ok(())
}

fn collect(
    value: &Value,
    now: i64,
    depth: usize,
    pairs: &mut BTreeMap<String, (String, Option<f64>)>,
) -> Result<()> {
    if depth > 8 {
        return Err(errors::invalid("豆包工作 Cookie 包装层数过多"));
    }
    own_provider(value)?;
    let source = ["cookies", "cookieHeader", "cookie", "token"]
        .iter()
        .find_map(|key| value.get(*key).filter(|value| !value.is_null()))
        .unwrap_or(value);
    if source.is_object()
        && ["cookies", "cookieHeader", "cookie", "token"]
            .iter()
            .any(|key| source.get(*key).is_some_and(|value| !value.is_null()))
    {
        return collect(source, now, depth + 1, pairs);
    }
    match source {
        Value::String(raw) => {
            let raw = raw.trim();
            if raw.len() > MAX_INPUT {
                return Err(errors::invalid("豆包工作 Cookie 文本过长"));
            }
            if raw.starts_with('{') || raw.starts_with('[') {
                let nested: Value = serde_json::from_str(raw)
                    .map_err(|_| errors::invalid("豆包工作 Cookie JSON 无法解析"))?;
                return collect(&nested, now, depth + 1, pairs);
            }
            let raw = raw.strip_prefix("Cookie:").unwrap_or(raw).trim();
            if raw.len() > MAX_HEADER {
                return Err(errors::invalid("豆包工作 Cookie 超过 64 KiB"));
            }
            for part in raw
                .split(';')
                .map(str::trim)
                .filter(|part| !part.is_empty())
            {
                let (name, value) = part.split_once('=').ok_or_else(|| {
                    errors::invalid("请粘贴含 sessionid=… 的完整 Cookie，不能只填令牌值")
                })?;
                put(pairs, name.trim(), value.trim(), None)?;
            }
        }
        Value::Array(rows) => {
            if rows.len() > MAX_COOKIES {
                return Err(errors::invalid("豆包工作 Cookie 条数过多"));
            }
            for row in rows {
                if !row.is_object() {
                    return Err(errors::invalid("豆包工作 Cookie 列表项必须是对象"));
                }
                if let Some(domain) = row.get("domain").filter(|value| !value.is_null()) {
                    let domain = domain
                        .as_str()
                        .ok_or_else(|| errors::invalid("豆包工作 Cookie 域名格式无效"))?;
                    if !matches!(
                        domain.trim_start_matches('.'),
                        "doubao.com" | "www.doubao.com"
                    ) {
                        continue;
                    }
                }
                if let Some(path) = row.get("path").filter(|value| !value.is_null()) {
                    if path.as_str().is_none() {
                        return Err(errors::invalid("豆包工作 Cookie 路径格式无效"));
                    }
                    if path != "/" {
                        continue;
                    }
                }
                let expires = row
                    .get("expires")
                    .filter(|value| !value.is_null())
                    .or_else(|| row.get("expirationDate").filter(|value| !value.is_null()));
                let expires = expires
                    .map(|value| {
                        value
                            .as_f64()
                            .filter(|value| value.is_finite())
                            .ok_or_else(|| errors::invalid("豆包工作 Cookie 过期时间格式无效"))
                    })
                    .transpose()?;
                if expires.is_some_and(|expiry| expiry > 0.0 && expiry <= (now / 1000) as f64) {
                    continue;
                }
                let name = row["name"]
                    .as_str()
                    .ok_or_else(|| errors::invalid("豆包工作 Cookie 缺少名称"))?;
                let value = row["value"]
                    .as_str()
                    .ok_or_else(|| errors::invalid("豆包工作 Cookie 缺少内容"))?;
                put(pairs, name, value, expires.filter(|expiry| *expiry > 0.0))?;
            }
        }
        Value::Object(fields) => {
            if fields.get("name").is_some_and(Value::is_string)
                && fields.get("value").is_some_and(Value::is_string)
            {
                return collect(&json!([source]), now, depth + 1, pairs);
            }
            for (name, value) in fields {
                if META.contains(&name.as_str()) {
                    continue;
                }
                let value = value
                    .as_str()
                    .ok_or_else(|| errors::invalid("豆包工作 Cookie 对象的值必须是文本"))?;
                put(pairs, name, value, None)?;
            }
        }
        _ => return Err(errors::invalid("缺少豆包工作 Cookie 快照")),
    }
    Ok(())
}

impl Credentials {
    pub fn parse(value: &Value, now: i64) -> Result<Self> {
        if value.to_string().len() > MAX_INPUT {
            return Err(errors::invalid("豆包工作凭据文本过长"));
        }
        own_provider(value)?;
        if value["source"] == "desktop" && !snapshot_present(value) {
            return Err(gates::legacy());
        }
        let mut pairs = BTreeMap::new();
        collect(value, now, 0, &mut pairs)?;
        if !["sessionid", "sessionid_ss"]
            .iter()
            .any(|name| pairs.get(*name).is_some_and(|(value, _)| !value.is_empty()))
        {
            return Err(agent2api_wasm_sdk::Error::http(
                401,
                "豆包工作 Cookie 缺少有效 sessionid/sessionid_ss，请重新登录后导入",
            )
            .with_code("doubaowork_cookie_missing"));
        }
        let cookie_header = pairs
            .iter()
            .map(|(key, (value, _))| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("; ");
        if cookie_header.len() > MAX_HEADER {
            return Err(errors::invalid("豆包工作 Cookie 超过 64 KiB"));
        }
        let cookies = json!(pairs.iter().map(|(name, (value, expires))| {
            // 这里只是固定站点的发送快照，不是浏览器回写记录。
            json!({ "name": name, "value": value, "domain": ".doubao.com", "path": "/", "expires": expires })
        }).collect::<Vec<_>>());
        let mut metadata = json!({});
        for (field, cookies) in [
            ("deviceId", &["device_id"][..]),
            ("iid", &["iid"][..]),
            ("webId", &["web_id"][..]),
            ("fingerprint", &["s_v_web_id", "fp"][..]),
        ] {
            if let Some(found) = cookies
                .iter()
                .find_map(|key| pairs.get(*key).map(|(value, _)| value))
            {
                metadata[field] = json!(found);
            }
        }
        for field in [
            "deviceId",
            "iid",
            "webId",
            "fingerprint",
            "clientVersion",
            "channel",
            "userAgent",
            "chromiumVersion",
            "teaUuid",
            "webTabId",
        ] {
            if let Some(raw) = value.get(field).filter(|value| !value.is_null()) {
                let raw = raw
                    .as_str()
                    .ok_or_else(|| errors::invalid("豆包工作客户端元数据必须是文本"))?;
                if raw.len() > 1024 || raw.chars().any(char::is_control) {
                    return Err(errors::invalid("豆包工作客户端元数据无效或过长"));
                }
                if !raw.trim().is_empty() {
                    let raw = raw.trim();
                    metadata[field] = json!(raw);
                }
            }
        }
        if let Some(device) = metadata["deviceId"]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_string)
        {
            if !util::is_numeric_id(&device) {
                // 非纯数字的设备标识（如浏览器 UUID）会被上游风控拒绝，留空交由启动配置自动补齐。
                if let Some(fields) = metadata.as_object_mut() {
                    fields.remove("deviceId");
                }
            }
        }
        let mut protected: Vec<String> = pairs
            .iter()
            .filter(|(name, (value, _))| {
                let trivial = matches!(
                    value.to_ascii_lowercase().as_str(),
                    "true" | "false" | "0" | "1" | "yes" | "no" | "null" | "none"
                );
                !trivial
                    && (matches!(name.as_str(), "sessionid" | "sessionid_ss") || value.len() >= 8)
            })
            .map(|(_, (value, _))| value.clone())
            .filter(|value| !value.is_empty())
            .collect();
        protected.push(cookie_header.clone());
        for field in ["deviceId", "iid", "webId", "fingerprint", "teaUuid", "webTabId"] {
            if let Some(raw) = metadata[field].as_str().filter(|raw| !raw.is_empty()) {
                protected.push(raw.into());
            }
        }
        Ok(Self {
            cookie_header,
            cookies,
            metadata,
            protected,
        })
    }

    pub fn has_numeric_device_id(&self) -> bool {
        util::is_numeric_id(self.field("deviceId"))
    }

    pub fn with_runtime_device(&self, device_id: Option<&str>) -> Self {
        if self.has_numeric_device_id() {
            return self.clone();
        }
        let Some(device_id) = device_id.filter(|id| util::is_numeric_id(id)) else {
            return self.clone();
        };
        let mut next = self.clone();
        next.metadata["deviceId"] = json!(device_id);
        if next.field("webId").is_empty() {
            next.metadata["webId"] = json!(device_id);
        }
        if !next.protected.iter().any(|item| item == device_id) {
            next.protected.push(device_id.to_string());
        }
        next
    }

    pub fn chat_context(&self) -> Result<()> {
        if !self.has_numeric_device_id() {
            return Err(errors::device_context());
        }
        Ok(())
    }

    pub fn field(&self, field: &str) -> &str {
        self.metadata[field].as_str().unwrap_or("")
    }

    pub fn fingerprint(&self) -> String {
        util::digest(
            json!({ "cookie": self.cookie_header, "cookies": self.cookies, "metadata": self.metadata })
                .to_string()
                .as_bytes(),
        )
    }

    pub fn tail(&self) -> String {
        let session = self
            .cookies
            .as_array()
            .into_iter()
            .flatten()
            .find(|row| {
                row["name"] == "sessionid"
                    && row["value"].as_str().is_some_and(|value| !value.is_empty())
            })
            .or_else(|| {
                self.cookies
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|row| row["name"] == "sessionid_ss")
            })
            .and_then(|row| row["value"].as_str())
            .unwrap_or("");
        if session.chars().count() <= 6 {
            return String::new();
        }
        let tail = session
            .chars()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        if self
            .protected
            .iter()
            .any(|secret| !secret.is_empty() && tail.contains(secret))
        {
            String::new()
        } else {
            tail
        }
    }

    pub fn store(&self) -> Value {
        let mut value = self.metadata.clone();
        value["cookies"] = self.cookies.clone();
        value
    }
}
