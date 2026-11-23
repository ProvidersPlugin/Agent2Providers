//! 保留原生四档分类顺序：内容审核 → 登录失效 → 余额／速率 → 原样失败。
//! 仅处理错误信封；不把正常模型答复中的“余额不足”字样误判成错误。

use super::client;
use agent2api_wasm_sdk::{self as sdk, json, Error, Value};

fn kind(body: &Value) -> &str {
    [
        body.pointer("/error/code"),
        body.pointer("/error/type"),
        body.get("providerCode"),
        body.get("code"),
        body.get("type"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .find(|value| !value.is_empty())
    .unwrap_or("")
}

fn number(body: &Value) -> Option<i64> {
    [
        &body["error"]["code"],
        &body["error"]["upstream_code"],
        &body["code"],
        &body["upstreamCode"],
        &body["upstream_code"],
    ]
    .into_iter()
    .find_map(|value| {
        value
            .as_i64()
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
    })
}

pub fn classify(status: u16, body: &Value) -> Value {
    let message = client::message(body);
    let lower = message.to_ascii_lowercase();
    let kind = kind(body);
    let content = matches!(
        kind,
        "content_filter" | "content_policy_violation" | "quwork_content"
    ) || lower.contains("content policy")
        || message.contains("内容安全")
        || message.contains("内容审核");
    let balance = matches!(
        kind,
        "insufficient_quota" | "insufficient_balance" | "quwork_balance"
    ) || [
        "余额不足",
        "积分不足",
        "额度不足",
        "额度已用完",
        "insufficient quota",
        "insufficient balance",
        "quota exceeded",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    let (class, output_status, note, reset) = if content {
        (
            "content_blocked",
            403,
            "QuWork 内容审核未通过，请调整提示内容",
            None,
        )
    } else if status == 401
        || matches!(
            kind,
            "invalid_api_key"
                | "token_expired"
                | "invalid_token"
                | "quwork_model_expired"
                | "quwork_account_expired"
        )
    {
        (
            "token_expired",
            401,
            "QuWork 凭证已失效，请重新登录或导入",
            None,
        )
    } else if balance || status == 402 {
        (
            "quota_limited",
            if status >= 400 { status } else { 402 },
            "QuWork 余额或积分不足",
            None,
        )
    } else if status == 429 || kind == "quwork_rate" {
        (
            "quota_limited",
            429,
            "QuWork 请求过于频繁，请稍后重试",
            Some(sdk::now().saturating_add(60_000)),
        )
    } else {
        (
            "fatal",
            status.clamp(400, 599),
            "QuWork 未接受本次请求",
            None,
        )
    };
    let reset = [
        &body["reset_at"],
        &body["resetAt"],
        &body["error"]["reset_at"],
        &body["error"]["resetAt"],
    ]
    .into_iter()
    .find_map(client::timestamp)
    .or(reset);
    json!({ "kind": class, "status": output_status, "message": note, "reset_at": reset, "upstream_code": number(body) })
}

pub fn upstream(status: u16, body: &Value, scope: &str) -> Error {
    let class = classify(status, body);
    let code = match class["kind"].as_str() {
        Some("content_blocked") => "quwork_content",
        Some("token_expired") if scope == "model" => "quwork_model_expired",
        Some("token_expired") => "quwork_account_expired",
        Some("quota_limited") if class["reset_at"].is_number() => "quwork_rate",
        Some("quota_limited") => "quwork_balance",
        _ if ["验证码", "人机", "captcha"]
            .iter()
            .any(|marker| client::message(body).contains(marker)) =>
        {
            "quwork_captcha"
        }
        _ => "quwork_upstream",
    };
    let message = if code == "quwork_captcha" {
        "QuWork 要求人机验证，请前往官方客户端完成"
    } else {
        class["message"].as_str().unwrap_or("QuWork 上游请求失败")
    };
    let mut error =
        Error::http(class["status"].as_u64().unwrap_or(502) as u16, message).with_code(code);
    error.reset_at = class["reset_at"].as_i64();
    error.upstream_code = class["upstream_code"].as_i64();
    error.credential_scope = Some(scope.into());
    error
}

pub fn account_expired() -> Error {
    let mut error = Error::http(401, "QuWork 账号会话已失效，请重新登录；模型凭据是独立凭据")
        .with_code("quwork_account_expired");
    error.credential_scope = Some("account".into());
    error
}

pub fn missing_account() -> Error {
    Error::http(
        400,
        "仅模型凭证可对话；积分、账号续期和奖励需要完整 accessToken 登录会话",
    )
    .with_code("usage_not_configured")
}

pub fn event(value: &Value) -> Option<Error> {
    if value.get("error").is_some_and(|error| !error.is_null())
        || value["type"] == "error"
        || value
            .get("success")
            .is_some_and(|value| !client::success(value))
    {
        let status = value["error"]["status"]
            .as_u64()
            .or_else(|| value["status"].as_u64())
            .filter(|status| (400..600).contains(status))
            .unwrap_or(502) as u16;
        return Some(upstream(status, value, "model"));
    }
    None
}

pub fn stream_error(error: &Error) -> Value {
    json!({ "error": { "message": error.message, "status": error.status.unwrap_or(502),
        "code": error.code, "type": "upstream_error", "upstream_code": error.upstream_code,
        "reset_at": error.reset_at, "credential_scope": error.credential_scope } })
}
