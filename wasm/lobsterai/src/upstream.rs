//! 地址来自旧 LobsterAI 实现。所有请求交给宿主，既不读环境变量，也不另建出网通道。

use super::{errors, version};
use agent2api_wasm_sdk::{http, json, Error, Result, Value};

pub const API: &str = "https://lobsterai-server.youdao.com";
pub const PORTAL: &str = "https://lobsterai.youdao.com";
pub const VERSION_URL: &str =
    "https://api-overmind.youdao.com/openapi/get/luna/hardware/lobsterai/prod/update";
pub const OFFICIAL_KEYFROM: &str = "official";
pub const ELECTRON_VERSION: &str = "43.5.0";
pub const CHROMIUM_VERSION: &str = "150.0.0.0";
pub const PLATFORM_TOKEN: &str = "Windows NT 10.0; Win64; x64";
pub const CAPABILITIES: &str =
    "virtual-models-v1,kimi-k3-agentic-v1,thinking-level-control-v1,upstream-error-details-v1";

pub fn user_agent(client_version: Option<&str>) -> String {
    let resolved = client_version
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(version::FALLBACK);
    format!(
        "Mozilla/5.0 ({PLATFORM_TOKEN}) AppleWebKit/537.36 (KHTML, like Gecko) \
         LobsterAI/{resolved} Chrome/{CHROMIUM_VERSION} Electron/{ELECTRON_VERSION} Safari/537.36"
    )
}

pub fn headers(token: Option<&str>, client_version: Option<&str>, include_capabilities: bool) -> Value {
    let mut headers = json!({
        "accept": "application/json",
        "content-type": "application/json",
        "user-agent": user_agent(client_version)
    });
    if let Some(token) = token {
        headers["authorization"] = json!(format!("Bearer {token}"));
    }
    if include_capabilities {
        if let Some(version) = client_version.map(str::trim).filter(|text| !text.is_empty()) {
            headers["x-lobsterai-client-capabilities"] = json!(CAPABILITIES);
            headers["x-lobsterai-client-version"] = json!(version);
        }
    }
    headers
}

pub fn transport(mut error: Error) -> Error {
    // 宿主的传输诊断可能含目录 URL 的 keyfrom 查询串，不能直接向用户回显。
    let (status, message) = match error.code.as_deref() {
        Some("timeout") => (504, "LobsterAI 上游连接或响应等待超时"),
        Some("aborted") => (499, "LobsterAI 请求已取消"),
        Some("forbidden") => (403, "LobsterAI 请求未获所需的主程序权限"),
        _ => (502, "LobsterAI 网络请求未完成，请检查账号代理或稍后重试"),
    };
    error.status = Some(status);
    error.message = message.to_string();
    error
}

pub fn read_json(response: http::Response) -> Result<Value> {
    let status = response.status;
    let bytes = response.bytes(4 * 1024 * 1024).map_err(transport)?;
    let parsed = serde_json::from_slice::<Value>(&bytes);
    if !(200..300).contains(&status) {
        let body = parsed.unwrap_or_else(|_| json!({ "message": String::from_utf8_lossy(&bytes) }));
        return Err(errors::upstream(status, &body));
    }
    parsed.map_err(|_| Error::http(502, "LobsterAI 接口未返回有效 JSON"))
}

pub fn request(
    method: &str,
    path: &str,
    token: Option<&str>,
    version: Option<&str>,
    include_capabilities: bool,
    body: Option<&Value>,
) -> Result<Value> {
    let response = http::open_timeout(
        method,
        &format!("{API}{path}"),
        headers(token, version, include_capabilities),
        body,
        30_000,
    )
    .map_err(transport)?;
    read_json(response)
}

pub fn anonymous(path: &str, body: &Value, version: &str) -> Result<Value> {
    request("POST", path, None, Some(version), false, Some(body))
}

pub fn envelope(value: &Value, operation: &str) -> Result<Value> {
    let code = errors::code(value)
        .ok_or_else(|| Error::http(502, format!("LobsterAI {operation}响应缺少业务状态")))?;
    if code != 0 {
        return Err(errors::upstream(
            if (400..600).contains(&code) {
                code as u16
            } else {
                502
            },
            value,
        ));
    }
    value
        .get("data")
        .filter(|data| !data.is_null())
        .cloned()
        .ok_or_else(|| errors::session("LobsterAI 登录状态已失效，请重新登录"))
}

pub fn get(path: &str, token: &str, version: &str, operation: &str) -> Result<Value> {
    envelope(
        &request("GET", path, Some(token), Some(version), false, None)?,
        operation,
    )
}

pub fn post(path: &str, token: &str, version: &str, body: &Value, operation: &str) -> Result<Value> {
    envelope(
        &request("POST", path, Some(token), Some(version), false, Some(body))?,
        operation,
    )
}

pub fn query(value: &str) -> String {
    let mut result = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            result.push(byte as char);
        } else {
            result.push_str(&format!("%{byte:02X}"));
        }
    }
    result
}
