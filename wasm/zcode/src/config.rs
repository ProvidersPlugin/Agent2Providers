use super::{
    account,
    network::{self, MONITOR, OPENAI, ZCODE},
};
use agent2api_wasm_sdk::{self as sdk, json, state, Error, Result, Value};

pub const DEFAULT_VERSION: &str = "3.18.2";
pub const PLATFORM: &str = "win32-x64";
pub const OS_CATEGORY: &str = "windows";
const CACHE_NAMESPACE: &str = "zcode-configuration";

pub fn app_version() -> Result<String> {
    let value = sdk::settings::get("appVersion")?;
    let version = value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_VERSION);
    if version.len() > 32
        || !version.as_bytes().first().is_some_and(u8::is_ascii_digit)
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Err(Error::http(400, "ZCode 客户端版本设置格式无效"));
    }
    Ok(version.to_string())
}

pub fn identity_headers(version: &str, suffix: Option<&str>) -> Value {
    json!({ "HTTP-Referer": ZCODE, "User-Agent": match suffix {
            Some(suffix) => format!("ZCode/{version} {suffix}"), None => format!("ZCode/{version}") },
        "X-ZCode-App-Version": version, "X-Title": "Z Code@cli", "X-Release-Channel": "production",
        "X-Client-Language": "unknown", "X-Client-Timezone": "unknown", "X-ZCode-Agent": "glm",
        "X-Platform": PLATFORM, "X-Os-Category": OS_CATEGORY })
}

pub fn billing_headers(jwt: &str, mid: &str) -> Value {
    let mut headers = json!({ "Accept": "application/json" });
    if !jwt.is_empty() {
        headers["Authorization"] = json!(format!("Bearer {jwt}"));
    }
    if !mid.is_empty() {
        headers["X-Device-Mid"] = json!(mid);
    }
    headers
}

pub fn billing_url(path: &str, version: &str) -> String {
    format!(
        "{ZCODE}/api/v1/zcode-plan/billing/{path}?app_version={}&platform={PLATFORM}",
        network::query(version)
    )
}

pub fn transient(key: &str) -> Result<state::Snapshot> {
    state::conversation_get(CACHE_NAMESPACE, key, 1)
}

pub fn put_transient(key: &str, before: &state::Snapshot, value: Value, ttl: i64) -> Result<bool> {
    state::conversation_compare_exchange(CACHE_NAMESPACE, key, 1, before.revision, value, ttl)
}

pub fn model_verification_binding(record: &Value) -> String {
    network::hash(&json!(["zcode", record["id"], record["identity"], record["secret"]]).to_string())
}

pub fn mapped_url(original: &str, record: &Value) -> Result<String> {
    if sdk::settings::get("disableEndpointMapping")? == true {
        return Ok(original.to_string());
    }
    let token = account::access(&record["secret"]);
    if token.is_empty() {
        return Ok(original.to_string());
    }
    let binding = model_verification_binding(record);
    let mut cache = transient("endpoint-mapping")?;
    let version = app_version()?;
    let mut value = cache.value.clone();
    let now = sdk::now();
    if !cache.compatible
        || value["binding"] != binding
        || value["version"] != version
        || value["expiresAt"].as_i64().unwrap_or(0) <= now
    {
        let mut headers = identity_headers(&version, None);
        headers["x-api-key"] = json!(token);
        let request = network::request(
            "GET",
            &format!(
                "{ZCODE}/api/v1/agent/configs?app_version={}&platform={PLATFORM}",
                network::query(&version)
            ),
            headers,
            None,
        );
        match request {
            Ok(reply) if (200..300).contains(&reply.status) && reply.value.is_object() => {
                let mut mappings = serde_json::Map::new();
                for entry in reply.value["data"]["proxyEndpoint"]["mapping"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(256)
                {
                    let from = entry["from"].as_str().and_then(routing_key);
                    let to = entry["to"].as_str().and_then(target);
                    if let (Some(from), Some(to)) = (from, to) {
                        mappings.insert(from, json!(to));
                    }
                }
                value = json!({ "binding": binding, "version": version, "expiresAt": now + 300_000, "mappings": mappings });
            }
            Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
            _ => {
                if !value.is_object() || value["binding"] != binding {
                    value = json!({ "binding": binding, "mappings": {} });
                }
                value["version"] = json!(version);
                value["expiresAt"] = json!(now + 60_000);
            }
        }
        account::require_current(record)?;
        if !put_transient("endpoint-mapping", &cache, value.clone(), 86_400_000)? {
            cache = transient("endpoint-mapping")?;
            value = cache.value;
        }
    }
    account::require_current(record)?;
    if value["binding"] != binding {
        return Ok(original.to_string());
    }
    let key = routing_key(original);
    let mapped = key.and_then(|key| value["mappings"][key].as_str().map(str::to_string));
    let Some(mapped) = mapped else {
        return Ok(original.to_string());
    };
    let mut parsed = url::Url::parse(&mapped).map_err(|_| Error::new("ZCode 端点映射格式无效"))?;
    let source = url::Url::parse(original).map_err(|_| Error::new("ZCode 请求地址格式无效"))?;
    parsed.set_query(source.query());
    Ok(parsed.to_string())
}

fn allowed_url(raw: &str) -> Option<url::Url> {
    let parsed = url::Url::parse(raw.trim()).ok()?;
    if parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || ![ZCODE, MONITOR].contains(&parsed.origin().ascii_serialization().as_str())
    {
        return None;
    }
    Some(parsed)
}
fn routing_key(raw: &str) -> Option<String> {
    let parsed = allowed_url(raw)?;
    let path = parsed.path().trim_end_matches('/');
    Some(format!(
        "https://{}:443{}",
        parsed.host_str()?,
        if path.is_empty() { "/" } else { path }
    ))
}
fn target(raw: &str) -> Option<String> {
    let mut parsed = allowed_url(raw)?;
    parsed.set_query(None);
    parsed.set_fragment(None);
    let path = parsed.path().trim_end_matches('/').to_string();
    parsed.set_path(if path.is_empty() { "/" } else { &path });
    Some(parsed.to_string())
}

pub fn schema() -> Value {
    json!({ "version": 1, "title": "ZCode 国内版活动与套餐", "runtime_overrides": false,
        "description": "WASM 使用独立设置，不读取环境变量；活动版本不改变套餐池推理模板绑定的版本。",
        "fields": [
            { "key": "appVersion", "label": "客户端版本号", "type": "upstream_version", "source_id": "zcode", "default": DEFAULT_VERSION,
                "description": "自动从官网同步最新正式版本，无需手动填写；断网或尚未同步时使用插件内置保底版本。" },
            { "key": "disableEndpointMapping", "label": "停用上游端点映射", "type": "boolean", "default": false },
            { "key": "requireCaptcha", "label": "套餐池模型请求始终要求验证", "type": "boolean", "default": false }
        ] })
}

pub fn coding_url() -> String {
    format!("{OPENAI}/chat/completions")
}
