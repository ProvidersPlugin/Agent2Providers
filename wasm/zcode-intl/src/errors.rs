use super::network::{integer, text};
use agent2api_wasm_sdk::{json, Error, Value};

pub fn code(body: &Value) -> Option<i64> {
    [
        body.get("upstreamCode"),
        body.get("upstream_code"),
        body.get("code"),
        body.pointer("/error/upstreamCode"),
        body.pointer("/error/upstream_code"),
        body.pointer("/error/code"),
    ]
    .into_iter()
    .flatten()
    .find_map(integer)
}
pub fn has_error(body: &Value) -> bool {
    body["type"] == "error"
        || body.get("error").is_some_and(|error| !error.is_null())
        || code(body).is_some_and(|code| code != 0)
}
fn message(body: &Value) -> String {
    let value = text(body, &["message", "msg"]);
    if value.is_empty() {
        let nested = text(&body["error"], &["message", "msg"]);
        if nested.is_empty() {
            body.as_str().unwrap_or("").to_string()
        } else {
            nested
        }
    } else {
        value
    }
}
pub fn risk(status: u16, body: &Value) -> bool {
    match code(body) {
        Some(code) => code == 3012,
        None => {
            matches!(status, 403 | 405)
                && message(body)
                    .to_ascii_lowercase()
                    .contains("request has been blocked due to unusual activity")
        }
    }
}
fn reset(body: &Value) -> Option<i64> {
    [
        body.get("reset_at"),
        body.get("resetAt"),
        body.get("nextResetTime"),
        body.pointer("/error/reset_at"),
        body.pointer("/error/resetAt"),
    ]
    .into_iter()
    .flatten()
    .find_map(integer)
    .filter(|at| *at > 0)
    .map(|at| {
        if at > 100_000_000_000 {
            at
        } else {
            at.saturating_mul(1000)
        }
    })
}

pub fn classify(status: u16, body: &Value) -> Value {
    let code = code(body);
    let raw = message(body).to_ascii_lowercase();
    let blocked = body["code"] == "content_filter"
        || [
            "blocked by security policy",
            "unapproved channel",
            "illegal api invocation",
        ]
        .iter()
        .any(|pattern| raw.contains(*pattern));
    let failed = if (400..600).contains(&status) {
        status
    } else {
        502
    };
    let (kind, status, description) = if risk(status, body) {
        (
            "fatal",
            failed,
            "ZCode 上游风控拒绝（3012），不是额度耗尽；已停止本次重试，请到官方客户端核对",
        )
    } else if code == Some(3007) {
        (
            "fatal",
            failed,
            "ZCode 国际版上游要求新的官方人机验证；这不是账号失效或额度不足",
        )
    } else if status == 401 || code == Some(401) {
        (
            "token_expired",
            401,
            "ZCode 当前套餐通道的凭据已被拒绝，请重新登录",
        )
    } else if matches!(code, Some(3008 | 3009)) {
        (
            "quota_limited",
            429,
            "ZCode 当前账号或模型的并发已满，不代表套餐额度耗尽，请稍后重试",
        )
    } else if status == 429 || matches!(code, Some(1005 | 1113 | 1302 | 1303)) {
        (
            "quota_limited",
            429,
            "ZCode 当前套餐通道额度或速率窗口受限；Coding Plan 与 Start Plan 的额度不能互相替代",
        )
    } else if blocked {
        (
            "content_blocked",
            failed,
            "ZCode 上游内容策略拒绝，请调整系统提示或对话内容后重试",
        )
    } else if code == Some(3001) {
        (
            "fatal",
            failed,
            "ZCode 上游参数校验失败，请核对设备标识与请求格式",
        )
    } else {
        ("fatal", failed, "ZCode 上游请求未成功")
    };
    json!({ "kind": kind, "status": status, "message": description, "upstream_code": code, "reset_at": reset(body) })
}

pub fn failure(status: u16, body: &Value) -> Error {
    let classified = classify(status, body);
    let code = code(body);
    let mut error = Error::http(
        classified["status"].as_u64().unwrap_or(502) as u16,
        classified["message"]
            .as_str()
            .unwrap_or("ZCode 上游请求失败"),
    );
    error.code = Some(if classified["kind"] == "content_blocked" {
        "content_filter".into()
    } else {
        code.map(|value| value.to_string())
            .unwrap_or_else(|| "upstream_error".into())
    });
    error.upstream_code = code;
    error.reset_at = reset(body);
    error
}

pub fn is_error_response(params: &Value) -> bool {
    let status = params["status"].as_u64().unwrap_or(502);
    !(200..300).contains(&status)
        || params["headers"]["content-type"]
            .as_str()
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("application/json")
            })
}

pub fn event(error: &Error) -> Value {
    json!({ "error": { "message": error.message, "code": error.code, "upstreamCode": error.upstream_code,
        "status": error.status.unwrap_or(502), "type": if error.status == Some(429) { "rate_limit_error" }
        else if error.status == Some(401) { "authentication_error" } else { "upstream_error" } } })
}
