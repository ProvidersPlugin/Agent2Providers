//! 原生AStudio的401/403凭据失效、429/积分额度不足限流；只检查错误信封，不扫描回答文本。
//! 错误对外只返回固定文案和数字业务码，禁止透出token、手机号、验证码或密码。

use agent2api_wasm_sdk::{json, Error, Value};

pub fn code(value: &Value) -> Option<i64> {
    [
        value.get("code"),
        value.pointer("/error/code"),
        value.pointer("/response/error/code"),
        value.get("upstreamCode"),
        value.get("upstream_code"),
        value.pointer("/error/upstreamCode"),
    ]
    .into_iter()
    .flatten()
    .find_map(|value| {
        value
            .as_i64()
            .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
    })
}

fn message(value: &Value) -> String {
    [
        value.pointer("/response/error/message"),
        value.pointer("/error/message"),
        value.get("message"),
        value.get("desc"),
        value.get("msg"),
        value.get("error"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .find(|value| !value.trim().is_empty())
    .unwrap_or("")
    .to_ascii_lowercase()
}

pub fn failed(value: &Value) -> bool {
    value["flag"] == false
        || value["success"] == false
        || value.get("code").is_some_and(|value| {
            !value.is_null() && value.as_i64() != Some(0) && value.as_str() != Some("0")
        })
        || value
            .get("error")
            .is_some_and(|value| !value.is_null() && value.as_str() != Some("") && *value != false)
}

pub fn expired(scope: &str) -> Error {
    let mut error = Error::http(
        401,
        if scope == "model" {
            "AStudio 模型凭据已失效，请重新登录或导入；本家没有自动续期接口"
        } else {
            "AStudio 网页会话已失效，请重新登录；模型凭据与网页会话独立"
        },
    )
    .with_code(if scope == "model" {
        "astudio_model_expired"
    } else {
        "astudio_session_expired"
    });
    error.credential_scope = Some(scope.into());
    error
}

pub fn classify(status: u16, body: &Value) -> Value {
    let text = message(body);
    let marker = body["providerCode"]
        .as_str()
        .or_else(|| body["code"].as_str())
        .or_else(|| body["error"]["code"].as_str())
        .unwrap_or("");
    let content = marker == "astudio_content"
        || body
            .pointer("/error/code")
            .is_some_and(|value| *value == "content_filter")
        || body
            .pointer("/response/error/code")
            .is_some_and(|value| *value == "content_filter")
        || body
            .pointer("/response/incomplete_details/reason")
            .is_some_and(|value| *value == "content_filter");
    let (kind, status, description) = if marker == "astudio_banned" {
        ("fatal", 403, "AStudio 账号已被封禁，请前往官方渠道处理")
    } else if matches!(
        marker,
        "local_precondition"
            | "native_helper_unavailable"
            | "forbidden"
            | "plugin_resource_limit"
            | "aborted"
            | "timeout"
    ) {
        (
            "fatal",
            status.clamp(400, 599),
            "AStudio 当前操作缺少必要权限或条件",
        )
    } else if content {
        (
            "content_blocked",
            403,
            "AStudio 内容策略拒绝本次请求，请调整提示内容",
        )
    } else if marker == "astudio_quota"
        || [
            "quota",
            "balance",
            "credit",
            "积分",
            "额度",
            "rate limit",
            "too many requests",
        ]
        .iter()
        .any(|word| text.contains(word))
    {
        (
            "quota_limited",
            if status >= 400 { status } else { 429 },
            "AStudio 当前积分或额度不足，或请求过于频繁",
        )
    } else if code(body) == Some(11001001)
        || text.contains("upgrade to the latest version")
        || text.contains("客户端平台、版本")
    {
        (
            "fatal",
            400,
            "AStudio 上游拒绝了当前客户端版本标识，请更新 AStudio 插件",
        )
    } else if matches!(status, 401 | 403)
        || matches!(code(body), Some(80000 | 1002 | 1005))
        || matches!(marker, "astudio_model_expired" | "astudio_session_expired")
        || text.contains("重新登录")
    {
        (
            "token_expired",
            401,
            "AStudio 凭据已失效，请重新登录；没有自动续期接口",
        )
    } else if status == 429
        || marker == "astudio_quota"
        || [
            "quota",
            "balance",
            "credit",
            "积分",
            "额度",
            "rate limit",
            "too many requests",
        ]
        .iter()
        .any(|word| text.contains(word))
    {
        (
            "quota_limited",
            if status >= 400 { status } else { 429 },
            "AStudio 当前积分或额度不足，或请求过于频繁",
        )
    } else if ["context", "too long", "maximum length", "too large"]
        .iter()
        .any(|word| text.contains(word))
    {
        ("fatal", 400, "AStudio 请求超出模型上下文或长度限制")
    } else {
        (
            "fatal",
            status.clamp(400, 599),
            "AStudio 未接受本次请求，请检查账号或请求参数",
        )
    };
    json!({ "kind": kind, "status": status, "message": description, "reset_at": null, "upstream_code": code(body) })
}

pub fn upstream(status: u16, body: &Value, scope: &str) -> Error {
    let class = classify(status, body);
    let marker = match class["kind"].as_str() {
        Some("token_expired") => {
            let mut error = expired(scope);
            error.upstream_code = code(body);
            return error;
        }
        Some("quota_limited") => "astudio_quota",
        Some("content_blocked") => "astudio_content",
        _ if matches!(body["providerCode"].as_str(), Some("astudio_banned")) => "astudio_banned",
        _ => "astudio_upstream",
    };
    let mut error = Error::http(
        class["status"].as_u64().unwrap_or(502) as u16,
        class["message"].as_str().unwrap_or("AStudio 请求失败"),
    )
    .with_code(marker);
    error.upstream_code = code(body);
    error.credential_scope = Some(scope.into());
    error
}

pub fn event(value: &Value) -> Option<Error> {
    if failed(value)
        || value["type"] == "error"
        || value["type"] == "response.failed"
        || value
            .pointer("/response/error")
            .is_some_and(|value| !value.is_null())
        || matches!(
            value["response"]["status"].as_str(),
            Some("failed" | "cancelled" | "canceled")
        )
    {
        let status = [
            value.pointer("/response/error/status"),
            value.pointer("/error/status"),
            value.get("status"),
        ]
        .into_iter()
        .flatten()
        .find_map(|value| value.as_u64())
        .filter(|value| (400..600).contains(value))
        .unwrap_or(502) as u16;
        return Some(upstream(status, value, "model"));
    }
    None
}

pub fn transport(error: Error) -> Error {
    match error.code.as_deref() {
        Some("timeout" | "aborted" | "plugin_resource_limit" | "bad_params") => error,
        Some("forbidden") => {
            Error::http(403, "AStudio 请求缺少主程序授权").with_code("local_precondition")
        }
        _ => Error::http(502, "AStudio 网络请求失败，请检查网络或代理")
            .with_code("astudio_transport"),
    }
}

pub fn stream(error: &Error) -> Value {
    json!({ "error": { "message": error.message, "status": error.status.unwrap_or(502),
        "code": error.code, "upstreamCode": error.upstream_code, "resetAt": error.reset_at,
        "credentialScope": error.credential_scope, "type": "upstream_error" } })
}
