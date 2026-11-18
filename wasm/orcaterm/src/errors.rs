//! 四档错误由实际 HTTP / CGI 业务码判定，不把未知 LLM_ERROR 猜成月度额度耗尽。

use agent2api_wasm_sdk::{json, Error, Value};

fn description(body: &Value) -> &str {
    [
        "/message",
        "/Message",
        "/error/message",
        "/Error/Message",
        "/error",
    ]
    .iter()
    .find_map(|path| {
        body.pointer(path)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
    })
    .or_else(|| body.as_str())
    .unwrap_or("OrcaTerm 上游错误")
}

fn code(body: &Value) -> String {
    ["/Error/Code", "/error/code", "/Code", "/code"]
        .iter()
        .find_map(|path| body.pointer(path).filter(|value| !value.is_null()))
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| value.to_string())
        })
        .unwrap_or_default()
        .to_ascii_uppercase()
}

fn authentication(status: u16, body: &Value) -> bool {
    if status >= 500 {
        return false;
    }
    let code = code(body);
    let text = description(body).to_ascii_lowercase();
    status == 401
        || [
            "TOKEN_EXPIRED",
            "SESSION_EXPIRED",
            "INVALID_REFRESH_TOKEN",
            "PROVIDER_AUTH_EXPIRED",
        ]
        .contains(&code.as_str())
        || text.contains("token")
            && ["invalid", "expired", "失效", "过期"]
                .iter()
                .any(|part| text.contains(part))
}

pub fn classify(params: &Value) -> Value {
    let status = params["status"]
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or(502);
    classified(status, &params["body"], &[])
}

fn classified(status: u16, body: &Value, credentials: &[&str]) -> Value {
    let message = message(description(body), credentials);
    if authentication(status, body) {
        return json!({ "kind": "token_expired", "message": message });
    }
    if status == 429 {
        return json!({ "kind": "quota_limited", "status": status, "message": message,
            "upstream_code": body["code"].as_i64(), "reset_at": null });
    }
    json!({ "kind": "fatal", "status": if (400..600).contains(&status) { status } else { 502 },
        "message": message, "upstream_code": body.get("Code").or_else(|| body.get("code")).and_then(Value::as_i64) })
}

pub fn business_failed(body: &Value) -> bool {
    body.get("Error")
        .or_else(|| body.get("error"))
        .is_some_and(|error| !error.is_null())
        || body
            .get("Code")
            .or_else(|| body.get("code"))
            .is_some_and(|value| !value.is_null() && value != 0 && value != "0")
}

pub fn upstream(status: u16, body: &Value, credentials: &[&str]) -> Error {
    let class = classified(status, body, credentials);
    let status = if class["kind"] == "token_expired" {
        401
    } else {
        class["status"].as_u64().unwrap_or(502) as u16
    };
    let mut error = Error::http(
        status,
        class["message"].as_str().unwrap_or("OrcaTerm 请求失败"),
    )
    .with_code("orcaterm_upstream");
    error.upstream_code = class["upstream_code"].as_i64();
    error
}

pub fn is_auth(error: &Error) -> bool {
    error.status == Some(401)
}

pub fn message(raw: &str, credentials: &[&str]) -> String {
    let mut redacted = raw.to_string();
    for credential in credentials
        .iter()
        .filter(|credential| !credential.is_empty())
    {
        redacted = redacted.replace(credential, "[已隐藏]");
    }
    redacted.chars().take(1000).collect()
}

pub fn event(error: &Error) -> Value {
    json!({ "error": { "message": error.message, "type": "proxy_error",
        "status": error.status.unwrap_or(502), "code": error.code, "upstream_code": error.upstream_code } })
}
