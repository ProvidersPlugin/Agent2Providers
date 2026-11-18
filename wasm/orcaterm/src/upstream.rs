//! 固定 CGI 与 LightAI 请求；出站代理、连接与取消由宿主提供。
//! 新客户端使用 orcaterm.com 的会话 Cookie，旧 CGI 仅在明确缺路由时兼容。

use agent2api_wasm_sdk::{http, json, Error, Result, Value};

use super::{credentials::Credentials, errors, CGI_BASE, USER_AGENT, WEB_BASE};

fn base_headers(accept: &str) -> Value {
    json!({ "Content-Type": "application/json", "Accept": accept, "User-Agent": USER_AGENT,
        "Origin": WEB_BASE, "Referer": format!("{WEB_BASE}/") })
}

// 1.6.6 客户端XHR使用官网会话Cookie，1.6.4 CGI仍使用Bearer；按固定域分开。
fn legacy_cgi_headers(credentials: Option<&Credentials>) -> Value {
    let mut headers = base_headers("application/json");
    headers["Origin"] = json!("https://orcaterm.cloud.tencent.com");
    headers["Referer"] = json!("https://orcaterm.cloud.tencent.com/");
    if let Some(credentials) = credentials {
        headers["Authorization"] = json!(format!("Bearer {}", credentials.access_token));
    }
    headers
}

pub fn cgi_headers(credentials: Option<&Credentials>) -> Result<Value> {
    let mut headers = base_headers("application/json");
    if let Some(credentials) = credentials {
        headers["Cookie"] = json!(format!("ot_session={}", credentials.access_token));
    }
    headers["x-seqid"] = json!(super::credentials::random_uuid()?);
    headers["x-csrfcode"] = json!("");
    headers["x-referer"] = json!(format!("{WEB_BASE}/terminal"));
    Ok(headers)
}

pub fn assistant_headers(credentials: &Credentials, seq_id: &str, stream: bool) -> Value {
    let mut headers = base_headers(if stream {
        "text/event-stream"
    } else {
        "application/json"
    });
    headers["Authorization"] = json!(format!("Bearer {}", credentials.access_token));
    headers["Cookie"] = json!(format!("ot_session={}", credentials.access_token));
    headers["X-Product"] = json!("orcaterm");
    headers["x-seqid"] = json!(seq_id);
    headers
}

pub fn checked(
    response: http::Response,
    operation: &str,
    secrets: &[&str],
) -> Result<http::Response> {
    let status = response.status;
    if (200..300).contains(&status) {
        return Ok(response);
    }
    let bytes = response.bytes(64 * 1024).unwrap_or_default();
    let raw = String::from_utf8_lossy(&bytes);
    let body: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({ "message": raw }));
    let mut error = errors::upstream(status, &body, secrets);
    error.message = format!("{operation}返回 HTTP {status}：{}", error.message);
    Err(error)
}

pub fn open(
    url: &str,
    headers: Value,
    body: &Value,
    operation: &str,
    secrets: &[&str],
) -> Result<http::Response> {
    let response = http::open("POST", url, headers, Some(body)).map_err(|mut error| {
        error.message = format!("{operation}网络请求失败");
        error
    })?;
    checked(response, operation, secrets)
}

pub fn post_json(
    url: &str,
    headers: Value,
    body: &Value,
    operation: &str,
    secrets: &[&str],
) -> Result<Value> {
    let response =
        http::open_timeout("POST", url, headers, Some(body), 15000).map_err(|mut error| {
            error.message = format!("{operation}网络请求失败");
            error
        })?;
    let response = checked(response, operation, secrets)?;
    let bytes = response.bytes(1024 * 1024)?;
    serde_json::from_slice(&bytes)
        .map_err(|_| Error::http(502, format!("{operation}返回非 JSON 响应")))
}

pub fn cgi(credentials: Option<&Credentials>, action: &str, data: Value) -> Result<Value> {
    let suffix = credentials
        .map(|credentials| format!("&uin={}", encode(&credentials.user_id)))
        .unwrap_or_default();
    let token = credentials
        .map(|credentials| credentials.access_token.as_str())
        .unwrap_or("");
    let session_id = data["sessionId"].as_str().unwrap_or("");
    let body = json!({ "action": action, "data": data });
    let mut last_missing = None;
    for base in [WEB_BASE, "https://api.orcaterm.com", CGI_BASE] {
        let headers = if base == CGI_BASE {
            legacy_cgi_headers(credentials)
        } else {
            cgi_headers(credentials)?
        };
        let response = post_json(
            &format!("{base}/cgi/api?action={action}{suffix}"),
            headers,
            &body,
            "OrcaTerm 账号接口",
            &[token, session_id],
        );
        match response {
            Ok(response) => {
                return response["data"]["Response"]
                    .as_object()
                    .map(|fields| Value::Object(fields.clone()))
                    .ok_or_else(|| Error::http(502, "OrcaTerm 账号接口缺少业务响应"));
            }
            // 只在服务器明确没有该路由时兼容旧域；认证/5xx/超时不换域重放换码或续期。
            Err(error) if matches!(error.status, Some(404 | 410)) => last_missing = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last_missing.unwrap_or_else(|| Error::http(502, "OrcaTerm 账号接口不可用")))
}

pub fn user_info(credentials: &Credentials) -> Result<(String, String)> {
    let response = cgi(Some(credentials), "GetUserInfo", json!({}))?;
    if errors::business_failed(&response) {
        return Err(errors::upstream(
            200,
            &response,
            &[&credentials.access_token],
        ));
    }
    let user_id = super::credentials::identifier(&response, &["userId", "uid"]);
    let nickname = super::credentials::text(&response, &["nickName", "username", "displayName"]);
    Ok((user_id, nickname))
}

pub fn encode(raw: &str) -> String {
    let mut output = String::new();
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(char::from(byte));
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}
