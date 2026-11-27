//! 保留原生认证业务码、HTTP 429 与内容策略分类，200 JSON 信封也必须报失败。

use agent2api_wasm_sdk::{json, Error, Value};

pub const SUCCESS: &str = "000000";

pub fn status(params: &Value) -> u16 {
    params["status"]
        .as_u64()
        .and_then(|status| u16::try_from(status).ok())
        .unwrap_or(502)
}

pub fn business_code(payload: &Value) -> String {
    for value in [
        payload.get("code"),
        payload.pointer("/error/code"),
        payload.get("upstream_code"),
    ] {
        match value {
            Some(Value::String(code)) if !code.trim().is_empty() => return code.trim().to_string(),
            Some(Value::Number(code)) if code.as_i64() == Some(0) => return SUCCESS.into(),
            Some(Value::Number(code)) => return code.to_string(),
            _ => {}
        }
    }
    String::new()
}

pub fn is_auth(code: &str) -> bool {
    matches!(code, "020002" | "100002" | "20002")
}

pub fn message(payload: &Value) -> String {
    [
        payload.pointer("/error/message"),
        payload.get("desc"),
        payload.get("message"),
        payload.get("error_msg"),
        payload.get("error"),
    ]
    .into_iter()
    .find_map(|value| {
        value
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    })
    .unwrap_or("")
    .chars()
    .take(3000)
    .collect()
}

pub fn redacted(text: &str, secrets: &[&str]) -> String {
    let mut result = text.to_string();
    for secret in secrets.iter().filter(|value| !value.is_empty()) {
        result = result.replace(secret, "[已隐藏]");
    }
    result.chars().take(3000).collect()
}

pub fn classify(status: u16, body: &Value) -> Value {
    let raw = message(body);
    let message = if raw.is_empty() {
        format!("上游返回 {status}：Loomy 未返回有效的流式响应")
    } else if raw.starts_with("上游返回 ") {
        raw.clone()
    } else {
        format!("上游返回 {status}: {raw}")
    };
    let code = business_code(body);
    let upstream_code = code.parse::<i64>().ok();
    // WASM 错误回传后，宿主把业务码保存在数值 code；保留旧前导零码的语义。
    let authentication = body.pointer("/error/type").and_then(Value::as_str)
        == Some("authentication_error")
        || body["providerCode"] == "loomy_authentication";
    if status == 401 || is_auth(&code) || authentication {
        return json!({ "kind": "token_expired", "message": message });
    }
    let lower = raw.to_lowercase();
    let balance = status == 402
        || body["providerCode"] == "loomy_balance"
        || matches!(status, 400..=599)
            && [
                "积分余额不足",
                "积分不足",
                "余额不足，请获取积分",
                "请获取积分后再试",
            ]
            .iter()
            .any(|pattern| raw.contains(pattern));
    if balance {
        return json!({ "kind": "quota_limited", "status": 402, "message": message,
            "upstream_code": upstream_code, "reset_at": null });
    }
    if status == 429 {
        return json!({ "kind": "quota_limited", "status": 429, "message": message,
            "upstream_code": upstream_code, "reset_at": null });
    }
    let status = if (200..300).contains(&status) {
        502
    } else {
        status
    };
    if status >= 400
        && [
            "blocked by security policy",
            "unapproved channel",
            "illegal api invocation",
        ]
        .iter()
        .any(|pattern| lower.contains(pattern))
    {
        return json!({ "kind": "content_blocked", "status": status, "upstream_code": upstream_code,
            "message": format!("{message}；内容被上游内容策略拒绝，这不是账号问题，请调整系统提示词或内容后重试") });
    }
    json!({ "kind": "fatal", "status": status, "message": message, "upstream_code": upstream_code })
}

pub fn http_error(status: u16, payload: &Value, secrets: &[&str]) -> Error {
    let class = classify(status, payload);
    let status = if class["kind"] == "token_expired" {
        401
    } else {
        class["status"]
            .as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .filter(|n| (400..600).contains(n))
            .unwrap_or(502)
    };
    let message = redacted(
        class["message"].as_str().unwrap_or("Loomy 上游请求失败"),
        secrets,
    );
    let mut error = Error::http(status, message).with_code(match class["kind"].as_str() {
        Some("token_expired") => "loomy_authentication",
        Some("quota_limited") if status == 402 => "loomy_balance",
        Some("quota_limited") => "loomy_quota",
        Some("content_blocked") => "loomy_content_blocked",
        _ => "loomy_upstream",
    });
    error.upstream_code = business_code(payload).parse().ok();
    error
}

pub fn is_error_response(status: u16, headers: &Value) -> bool {
    if !(200..300).contains(&status) {
        return true;
    }
    headers
        .as_object()
        .into_iter()
        .flatten()
        .find(|(key, _)| key.eq_ignore_ascii_case("content-type"))
        .and_then(|(_, value)| value.as_str())
        .is_some_and(|value| {
            let mime = value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            mime == "application/json" || mime.ends_with("+json")
        })
}

pub fn ensure_success(payload: &Value, operation: &str, session: &str) -> Result<(), Error> {
    let code = business_code(payload);
    if code == SUCCESS {
        return Ok(());
    }
    let mut error = http_error(if is_auth(&code) { 401 } else { 502 }, payload, &[session]);
    error.message = format!("{operation}失败：{}", error.message);
    Err(error)
}

pub fn event(error: &Error) -> Value {
    json!({ "error": { "message": error.message, "status": error.status.unwrap_or(502), "type": "upstream_error", "code": error.code } })
}
