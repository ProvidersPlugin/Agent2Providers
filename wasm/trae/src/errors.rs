//! Trae正式业务错误：4001/4023仅通道路由不匹配；1005/4008限额；
//! 401/1001/20101失效；4011/9074软拒绝。只检查错误信封，绝不扫描回答正文。
//! 对外固定脱敏文案+数字业务码，不回上游完整响应或携带凭据的URL。

use super::util;
use agent2api_wasm_sdk::{json, Error, Value};

pub fn code(value: &Value) -> Option<i64> {
    [
        value.pointer("/error/code"),
        value.pointer("/error/upstream_code"),
        value.get("code"),
        value.get("Code"),
        value.pointer("/ResponseMetadata/Error/Code"),
        value.pointer("/ResponseMetadata/Error/CodeN"),
        value.get("upstreamCode"),
    ]
    .into_iter()
    .flatten()
    .find_map(util::integer)
}

pub fn message(value: &Value) -> String {
    [
        value.pointer("/error/message"),
        value.get("message"),
        value.get("msg"),
        value.get("Message"),
        value.pointer("/ResponseMetadata/Error/Message"),
        value.get("__Message.error"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .find(|v| !v.is_empty())
    .unwrap_or("")
    .to_ascii_lowercase()
}

pub fn failed(value: &Value) -> bool {
    code(value).is_some_and(|c| c != 0)
        || [value.get("code"), value.get("Code")]
            .into_iter()
            .flatten()
            .any(|v| !v.is_null() && util::integer(v).is_none())
        || value
            .get("error")
            .is_some_and(|v| !v.is_null() && *v != false && v.as_str() != Some(""))
        || value
            .pointer("/ResponseMetadata/Error")
            .is_some_and(|v| !v.is_null())
        || value["success"] == false
}

fn input_too_large(text: &str) -> bool {
    [
        "too long",
        "too many tokens",
        "context length",
        "maximum context",
        "context window",
        "request entity too large",
        "input too large",
        "输入过长",
        "内容过长",
        "上下文过长",
        "上下文长度",
        "超出模型上限",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

pub fn upstream(status: u16, body: &Value) -> Error {
    let c = code(body);
    let text = message(body);
    let meta_code = body
        .pointer("/ResponseMetadata/Error/Code")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let marker = [
        body.get("providerCode"),
        body.pointer("/error/code"),
        body.get("code"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .find(|v| v.starts_with("trae_") || *v == "local_precondition")
    .unwrap_or("");
    let (status, marker, message) = if marker == "local_precondition" {
        (
            status.clamp(400, 599),
            "local_precondition",
            "Trae 当前操作缺少必要条件",
        )
    } else if status == 413 || marker == "trae_input_too_large" || input_too_large(&text) {
        (
            413,
            "trae_input_too_large",
            "Trae 请求超出模型上下文或输入长度限制",
        )
    } else if matches!(c, Some(20401 | 20405)) || marker == "trae_device_rejected" {
        (
            403,
            "trae_device_rejected",
            "Trae 设备绑定被拒绝或设备数已达上限，请在官方客户端确认",
        )
    } else if status == 401
        || matches!(c, Some(1001 | 401 | 20101))
        || marker == "trae_session_expired"
        || matches!(
            meta_code.as_str(),
            "unauthorized" | "invalidtoken" | "tokenexpired"
        )
        || text.contains("not able to authenticate")
        || text.contains("unable to authenticate")
        || text.contains("refresh token is invalid")
    {
        (
            401,
            "trae_session_expired",
            "Trae 登录凭据已被拒绝，需要续期或重新登录",
        )
    } else if matches!(c, Some(1005 | 4008)) || marker == "trae_plan_limit" {
        (429, "trae_plan_limit", "Trae 当前模型或套餐额度不足")
    } else if status == 429 || matches!(c, Some(4011 | 9074)) || marker == "trae_rate_limited" {
        (
            429,
            "trae_rate_limited",
            "Trae 暂时拒绝本次请求，请稍后手动重试",
        )
    } else if matches!(c, Some(4001 | 4023)) || marker == "trae_model_unavailable" {
        (
            404,
            "trae_model_unavailable",
            "Trae 当前目录通道不支持这个模型",
        )
    } else if c == Some(10101) {
        (
            400,
            "trae_client_mismatch",
            "Trae 续期凭据与登录应用归属不匹配",
        )
    } else if marker == "trae_content_blocked"
        || matches!(
            body.pointer("/error/code").and_then(Value::as_str),
            Some("content_filter" | "content_policy_violation")
        )
    {
        (403, "trae_content_blocked", "Trae 内容策略拒绝本次请求")
    } else {
        (
            if status >= 400 { status.min(599) } else { 502 },
            "trae_upstream",
            "Trae 上游未接受本次请求",
        )
    };
    let mut error = Error::http(status, message).with_code(marker);
    error.upstream_code = c;
    if marker == "trae_session_expired" {
        error.credential_scope = Some("account".into());
    }
    error
}

pub fn expired() -> Error {
    upstream(401, &json!({ "code": 1001 }))
}

pub fn classify(status: u16, value: &Value) -> Value {
    let error = upstream(status, value);
    let kind = match error.code.as_deref() {
        Some("trae_session_expired") => "token_expired",
        Some("trae_plan_limit" | "trae_rate_limited") => "quota_limited",
        Some("trae_content_blocked") => "content_blocked",
        _ => "fatal",
    };
    json!({ "kind": kind, "status": error.status, "message": error.message, "upstream_code": error.upstream_code, "reset_at": null })
}

pub fn transport(error: Error) -> Error {
    match error.code.as_deref() {
        Some("timeout") => Error::http(504, "Trae 请求超时").with_code("timeout"),
        Some("aborted") => Error::http(499, "Trae 请求已取消").with_code("aborted"),
        Some("forbidden") => {
            Error::http(403, "Trae 请求未获主程序授权").with_code("local_precondition")
        }
        _ => Error::http(502, "Trae 网络请求失败，请检查出口网络").with_code("trae_transport"),
    }
}

pub fn stream(error: &Error) -> Value {
    json!({ "error": { "message": error.message, "code": error.code, "upstream_code": error.upstream_code,
        "status": error.status.unwrap_or(502), "type": "upstream_error" } })
}

pub fn route_retry(error: &Error, original_status: u16) -> bool {
    !matches!(original_status, 401 | 403 | 429)
        && matches!(error.upstream_code, Some(4001 | 4023 | 1005))
        && error.code.as_deref() != Some("trae_input_too_large")
}

pub fn terminal_code(error: &Error) -> bool {
    error.status == Some(401)
        || error.status == Some(403)
        || error.status == Some(429)
        || error
            .code
            .as_deref()
            .is_some_and(|code| matches!(code, "local_precondition" | "aborted" | "timeout"))
}
