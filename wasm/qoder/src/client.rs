//! 账号/目录请求只走三个静态 HTTPS 来源；错误不回显令牌、PKCE 轮询 URL 或上游原文。
use super::{endpoints, errors, util};
use agent2api_wasm_sdk::{http, json, Error, Result, Value};

pub struct Reply {
    pub status: u16,
    pub value: Value,
}
pub fn url(origin: &str, path: &str) -> Result<String> {
    if ![endpoints::OPEN_API, endpoints::GATEWAY, endpoints::WEB].contains(&origin)
        || !path.starts_with('/')
        || path.starts_with("//")
        || path.contains(['\\', '\r', '\n', '\0', '#'])
    {
        return Err(Error::coded("Qoder 请求超出国内固定来源", "forbidden"));
    }
    Ok(format!("{origin}{path}"))
}
pub fn request(
    method: &str,
    origin: &str,
    path: &str,
    headers: Value,
    body: Option<&Value>,
    operation: &str,
) -> Result<Reply> {
    let url = url(origin, path)?;
    let result = (|| -> Result<Reply> {
        let response = http::open_timeout(method, &url, headers, body, 20_000)?;
        let status = response.status;
        let bytes = response.bytes(4 * 1024 * 1024)?;
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok(Reply { status, value })
    })();
    result.map_err(|mut error| {
        if !util::terminal(&error)
            && !matches!(
                error.code.as_deref(),
                Some("plugin_resource_limit" | "plugin_unavailable")
            )
        {
            error.message = format!("Qoder {operation}连接失败或响应中断");
            error.code = Some("qoder_transport".into());
        }
        error
    })
}
pub fn accepted(reply: Reply, action: &str) -> Result<Value> {
    if !(200..300).contains(&reply.status) {
        return Err(errors::failure(reply.status, &reply.value));
    }
    if !reply.value.is_object() {
        return Err(Error::http(
            502,
            format!("Qoder {action}未返回有效 JSON 对象"),
        ));
    }
    if errors::has_error(&reply.value) {
        return Err(errors::failure(reply.status, &reply.value));
    }
    Ok(reply.value)
}
pub fn account_headers(token: Option<&str>) -> Value {
    let mut headers = json!({ "Cosy-Version": "1.0.1", "Cosy-ClientType": "5",
        "User-Agent": "qoder-local-proxy", "Accept": "application/json", "Content-Type": "application/json" });
    if let Some(token) = token {
        headers["Authorization"] = json!(format!("Bearer {token}"));
    }
    headers
}
pub fn refresh_headers() -> Value {
    json!({ "User-Agent": "qoder-local-proxy", "Accept": "application/json", "Content-Type": "application/json" })
}
pub fn sash_headers(token: &str) -> Value {
    json!({ "authorization": format!("Bearer {token}"), "cosy-clienttype": "10", "accept": "application/json",
        "accept-language": "zh-CN", "user-agent": "Qoder", "content-type": "application/json" })
}
pub fn get(path: &str, token: &str, action: &str) -> Result<Reply> {
    request(
        "GET",
        endpoints::OPEN_API,
        path,
        account_headers(Some(token)),
        None,
        action,
    )
}
