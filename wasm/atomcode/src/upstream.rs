use agent2api_wasm_sdk::{self as sdk, http, json, Result, Value};

const USER_AGENT: &str = concat!("agent2api-atomcode/", env!("CARGO_PKG_VERSION"));

pub fn base(key: &str) -> String {
    let fallback = match key {
        "brokerBase" => "https://acs.atomgit.com",
        "codingplanBase" => "https://api.gitcode.com/api/v5",
        _ => "https://api-ai.gitcode.com/v1",
    };
    sdk::host("settings.get", json!({ "key": key })).ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .filter(|value| !value.trim().is_empty()).unwrap_or_else(|| fallback.to_string())
        .trim_end_matches('/').to_string()
}

pub fn headers(token: Option<&str>) -> Value {
    // 如实标识本网关；不能自称官方 atomcode 客户端（会切到其签名协议）。
    let mut headers = json!({ "accept": "application/json", "content-type": "application/json", "user-agent": USER_AGENT });
    if let Some(token) = token { headers["authorization"] = json!(format!("Bearer {token}")); }
    headers
}

pub fn get(path: &str, token: &str) -> Result<Value> {
    http::open("GET", &format!("{}{path}", base("codingplanBase")), headers(Some(token)), None)?.json()
}

pub fn post(path: &str, token: &str, body: &Value) -> Result<Value> {
    http::open("POST", &format!("{}{path}", base("codingplanBase")), headers(Some(token)), Some(body))?.json()
}

pub fn status(token: &str) -> Result<Value> { get("/coding-plan/status-v2", token) }

pub fn query(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) { output.push(byte as char); }
        else { output.push_str(&format!("%{byte:02X}")); }
    }
    output
}
