use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};

use super::{errors, signing};

pub fn base(key: &str) -> String {
    let fallback = match key {
        "brokerBase" => "https://acs.atomgit.com",
        "codingplanBase" => "https://api.gitcode.com/api/v5",
        _ => "https://llm-api.atomgit.com/v1",
    };
    sdk::host("settings.get", json!({ "key": key })).ok()
        .and_then(|value| value.as_str().map(str::trim).map(str::to_string))
        .filter(|value| {
            let Ok(url) = url::Url::parse(value) else { return false; };
            let Ok(expected) = url::Url::parse(fallback) else { return false; };
            url.origin() == expected.origin()
                && url.path().trim_end_matches('/') == expected.path().trim_end_matches('/')
                && url.query().is_none() && url.fragment().is_none()
                && url.username().is_empty() && url.password().is_none()
        })
        .unwrap_or_else(|| fallback.to_string()).trim_end_matches('/').to_string()
}

pub fn headers(token: Option<&str>) -> Value {
    let mut headers = json!({ "accept": "application/json", "content-type": "application/json",
        "user-agent": format!("atomcode/{}", signing::CLIENT_VERSION) });
    if let Some(token) = token { headers["authorization"] = json!(format!("Bearer {token}")); }
    headers
}

pub fn response_json(response: http::Response) -> Result<Value> {
    let status = response.status;
    let bytes = response.bytes(1024 * 1024)?;
    let parsed = serde_json::from_slice::<Value>(&bytes);
    if !(200..300).contains(&status) {
        return Err(errors::http_error(status, &parsed.unwrap_or(Value::Null)));
    }
    let value = parsed.map_err(|_| Error::http(502, "AtomCode 上游接口未返回有效 JSON"))?;
    if errors::business_error(&value) {
        return Err(errors::http_error(errors::status(&value).unwrap_or(502), &value));
    }
    Ok(value)
}

pub fn get(path: &str, token: &str) -> Result<Value> {
    response_json(http::open_timeout("GET", &format!("{}{path}", base("codingplanBase")),
        headers(Some(token)), None, 20_000)?)
}

pub fn status(token: &str) -> Result<Value> {
    let value = get("/coding-plan/status-v2", token)?;
    if !value.is_object() || !["codingplan_free", "plan", "plan_type", "rate_limit_windows", "current_usage"]
        .iter().any(|key| value.get(*key).is_some()) {
        return Err(Error::http(502, "AtomCode 套餐状态缺少有效字段，未将未知状态当成可领取"));
    }
    Ok(value)
}

pub fn query(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) { output.push(byte as char); }
        else { output.push_str(&format!("%{byte:02X}")); }
    }
    output
}
