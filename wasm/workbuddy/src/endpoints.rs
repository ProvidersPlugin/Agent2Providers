//! 国内当前版本的确切端点。不会把国际域/额度/保活协议通过字符串替换混入。
//! 来源 workbuddy_shared/endpoints.rs(5.9.3/CLI2.165.0)、growth和contract真实调用。
use agent2api_wasm_sdk::{Error, Result, Value};

pub const ID: &str = "workbuddy";
pub const ORIGIN: &str = "https://www.workbuddy.cn";
pub const CHAT: &str = "https://copilot.tencent.com";
pub const BILL: &str = "https://www.codebuddy.cn";
pub const BACKUP: &str = "https://wb.tencentbuddy.com";
pub const COS: &str = "https://acc-1258344699.cos.accelerate.myqcloud.com";
pub const STATIC: &str = "https://static.workbuddy.cn";
pub const HELPER: &str = "desktop.credentials.workbuddy.v1";
pub const AUTH: &str = "/v2/plugin";
pub const VERSION: &str = "5.9.3";
pub const UA: &str = "WorkBuddy/5.9.3 WorkBuddy/5.9.3 CLI/2.165.0";
pub const MAX_CREDENTIALS: usize = 256 * 1024;

// 宿主在唯一对话投递前检查固定官方入口；地址候选不改变域名、TLS或原代理出口。
pub fn network_policy() -> Value {
    agent2api_wasm_sdk::json!({
        "trustedOrigins": [ORIGIN, CHAT, BACKUP], "backupOrigin": BACKUP,
        "retryStatuses": [408, 502, 503, 504], "addressStatuses": [408, 502, 503],
        "httpDns": { "token": "719980335", "bootstrapIps": ["119.29.29.99", "119.28.28.99"] },
        "fallbackAddresses": ["162.14.3.241"], "maxAttempts": 4
    })
}

pub fn endpoint(secret: &Value) -> Result<String> {
    let raw = secret["endpoint"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(ORIGIN);
    let parsed =
        url::Url::parse(raw).map_err(|_| Error::http(400, "WorkBuddy 国内账号端点无效"))?;
    let origin = parsed.origin().ascii_serialization();
    if ![ORIGIN, CHAT, BILL, BACKUP].contains(&origin.as_str())
        || parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !matches!(parsed.path(), "" | "/")
    {
        return Err(Error::http(
            400,
            "WorkBuddy 国内版仅允许已确认的官方国内端点；国际账号请使用国际版插件",
        ));
    }
    Ok(origin)
}
pub fn url(origin: &str, path: &str) -> Result<String> {
    if ![ORIGIN, CHAT, BILL, BACKUP, COS, STATIC].contains(&origin)
        || !path.starts_with('/')
        || path.starts_with("//")
        || path.contains('\\')
        || path.contains('#')
        || path.chars().any(char::is_control)
    {
        return Err(Error::http(400, "WorkBuddy 请求不在已声明来源内"));
    }
    let url = url::Url::parse(&format!("{origin}{path}"))
        .map_err(|_| Error::http(400, "WorkBuddy 请求地址无效"))?;
    if url.origin().ascii_serialization() != origin {
        return Err(Error::http(400, "WorkBuddy 不允许跨域凭据请求"));
    }
    Ok(url.into())
}
pub fn encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
