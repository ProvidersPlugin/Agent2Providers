//! 原生Region::Intl实际取值，不用GLOBAL_ENDPOINT(www.codebuddy.ai)替换WorkBuddy国际主站。
//! 来源workbuddy_shared/endpoints.rs EDITION_INTL；后台与对话的UA顺序确有不同。
use agent2api_wasm_sdk::{Error, Result, Value};

pub const ID: &str = "workbuddy-intl";
pub const ORIGIN: &str = "https://www.workbuddy.ai";
pub const AUTH: &str = "/v2/plugin";
pub const PLATFORM: &str = "workbuddy-ai";
pub const VERSION: &str = "5.8.4";
pub const UA: &str = "workbuddy-ai/5.8.4 WorkBuddy/5.8.4 CLI/2.161.0";
pub const CHAT_UA: &str = "WorkBuddy/5.8.4 workbuddy-ai/5.8.4 CLI/2.161.0";
pub const WEB_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36";
pub const HELPER: &str = "desktop.credentials.workbuddy-intl.v1";
pub const MAX_CREDENTIALS: usize = 256 * 1024;

pub fn endpoint(secret: &Value) -> Result<String> {
    let raw = secret["endpoint"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(ORIGIN);
    let parsed =
        url::Url::parse(raw).map_err(|_| Error::http(400, "WorkBuddy国际版账号端点无效"))?;
    // 旧国际账号曾保存 CodeBuddy 的官方别名；同一国际 realm 现统一使用声明内主站。
    // 不接受国内、自建或预发布地址，也不扩大本包的网络来源。
    if !matches!(
        parsed.origin().ascii_serialization().as_str(),
        ORIGIN | "https://www.codebuddy.ai"
    ) || parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !matches!(parsed.path(), "" | "/")
    {
        return Err(Error::http(
            400,
            "WorkBuddy国际版仅允许已确认的官方国际主站；不接受国内、预发布或自建端点",
        ));
    }
    Ok(ORIGIN.into())
}
pub fn url(origin: &str, path: &str) -> Result<String> {
    if origin != ORIGIN
        || !path.starts_with('/')
        || path.starts_with("//")
        || path.contains('\\')
        || path.contains('#')
        || path.chars().any(char::is_control)
    {
        return Err(Error::http(400, "WorkBuddy国际请求不在已声明来源内"));
    }
    let parsed = url::Url::parse(&format!("{origin}{path}"))
        .map_err(|_| Error::http(400, "国际请求地址无效"))?;
    if parsed.origin().ascii_serialization() != ORIGIN {
        return Err(Error::http(400, "国际凭据不能跨域使用"));
    }
    Ok(parsed.into())
}
pub fn encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
