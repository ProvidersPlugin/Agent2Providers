use agent2api_wasm_sdk::{http, json, Error, Result, Value};

use crate::{credentials::Cookies, endpoints, errors};

pub fn web_headers(cookie: &Cookies) -> Value {
    let mut headers = json!({ "Cookie": cookie.header, "Accept": "application/json, text/plain, */*",
        "Accept-Language": "zh-CN,zh;q=0.9", "X-Dumate-Client-Type": "web",
        "Referer": format!("{}/app", cookie.origin), "Origin": cookie.origin,
        "Content-Type": "application/json",
        "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36" });
    if let Some(csrf) = cookie.values.get("bce-user-info") {
        headers["csrftoken"] = json!(csrf);
    }
    headers
}

pub fn gateway_headers(token: &str, tools: bool) -> Value {
    let mut headers = json!({ "Authorization": format!("Bearer {token}"),
        "Content-Type": "application/json", "Accept": "text/event-stream",
        "X-Dumate-Client-Type": "web", "X-Dumate-Account-Id": "global" });
    if tools {
        headers["X-Appbuilder-From"] = json!("openclaw");
    }
    headers
}

pub fn read(response: http::Response, protected: &[String]) -> Result<Value> {
    let status = response.status;
    let retry_after = response
        .header_values("retry-after")
        .first()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds <= 7 * 86_400);
    let bytes = response.bytes(4 * 1024 * 1024)?;
    let value = serde_json::from_slice::<Value>(&bytes).map_err(|_| {
        let status = if (400..600).contains(&status) {
            status
        } else {
            502
        };
        let error = Error::http(
            status,
            format!("百度搭子接口未返回有效 JSON（HTTP {status}），请核对登录状态"),
        );
        if status == 401 {
            error.with_code("dumate_session_expired")
        } else {
            error
        }
    })?;
    if !(200..300).contains(&status) || errors::failed(&value) {
        let mut error = errors::upstream(status, &value, protected);
        error.reset_at = crate::util::timestamp(&value["resetAt"])
            .or_else(|| crate::util::timestamp(&value["error"]["resetAt"]))
            .filter(|at| *at > agent2api_wasm_sdk::now());
        if matches!(
            error.code.as_deref(),
            Some("dumate_rate_limited" | "dumate_quota_exhausted")
        ) && error.reset_at.is_none()
        {
            error.reset_at = retry_after
                .and_then(|seconds| i64::try_from(seconds).ok())
                .and_then(|seconds| seconds.checked_mul(1000))
                .and_then(|milliseconds| crate::util::now().ok()?.checked_add(milliseconds));
        }
        return Err(error);
    }
    Ok(value)
}

pub fn data(value: Value) -> Value {
    value
        .get("result")
        .or_else(|| value.get("data"))
        .cloned()
        .unwrap_or(value)
}

pub fn web(
    method: &str,
    path: &str,
    cookie: &Cookies,
    body: Option<&Value>,
    protected: &[String],
) -> Result<Value> {
    let empty = json!({});
    let body = body.or_else(|| (method == "POST").then_some(&empty));
    let response = http::open_timeout(
        method,
        &format!("{}{path}", cookie.origin),
        web_headers(cookie),
        body,
        20_000,
    )?;
    read(response, protected).map(data)
}

pub fn gateway(
    method: &str,
    path: &str,
    token: &str,
    body: Option<&Value>,
    protected: &[String],
) -> Result<Value> {
    let mut headers = gateway_headers(token, true);
    headers["Accept"] = json!("application/json");
    let response = http::open_timeout(
        method,
        &format!("{}{path}", endpoints::GATEWAY),
        headers,
        body,
        30_000,
    )?;
    read(response, protected).map(data)
}
