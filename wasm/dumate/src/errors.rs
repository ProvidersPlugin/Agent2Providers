use agent2api_wasm_sdk::{json, Error, Value};

use crate::util;

pub fn code(value: &Value) -> Option<i64> {
    [value.get("error"), Some(value)]
        .into_iter()
        .flatten()
        .find_map(|value| {
            value["code"]
                .as_i64()
                .or_else(|| value["code"].as_str()?.parse().ok())
        })
}

pub fn failed(value: &Value) -> bool {
    value
        .get("error")
        .is_some_and(|error| !error.is_null() && error != false)
        || value["success"] == false
        || code(value).is_some_and(|code| code != 0)
        || value["code"].as_str().is_some_and(|code| {
            !code.trim().is_empty()
                && !matches!(
                    code.trim().to_ascii_lowercase().as_str(),
                    "0" | "success" | "ok"
                )
                && (value.get("message").is_some() || value.get("type").is_some())
        })
        || value["type"]
            .as_str()
            .is_some_and(|kind| kind == "error" || kind.ends_with("_error"))
}

fn message(value: &Value) -> String {
    for value in [value.get("error"), Some(value)].into_iter().flatten() {
        if let Some(text) = value.as_str() {
            return text.to_string();
        }
        let message = util::text(value, &["message", "msg", "error_message"]);
        if !message.is_empty() {
            return message;
        }
    }
    "上游未提供具体原因".into()
}

fn kind(status: u16, value: &Value) -> &'static str {
    let lower = message(value).to_ascii_lowercase();
    let error_type = [
        util::text(&value["error"], &["type", "code"]),
        util::text(value, &["type", "providerCode", "code"]),
    ]
    .join(" ")
    .to_ascii_lowercase();
    let contains = |markers: &[&str]| {
        markers
            .iter()
            .any(|marker| lower.contains(marker) || error_type.contains(marker))
    };
    if contains(&[
        "datainspectionfailed",
        "inappropriate content",
        "content_filter",
        "contentfilter",
        "sensitivecontent",
        "content_policy",
        "dumate_content_blocked",
        "内容审核",
        "敏感内容",
    ]) {
        "content_blocked"
    } else if contains(&[
        "invalid_model",
        "invalid model",
        "model not found",
        "not registered",
        "prompt 超长",
        "context_length_exceeded",
    ]) {
        "fatal"
    } else if matches!(code(value), Some(10001 | 10006))
        || status == 401
        || contains(&[
            "authentication_error",
            "invalid_token",
            "token expired",
            "登录态已失效",
            "dumate_session_expired",
        ])
    {
        "token_expired"
    } else if status == 402
        || contains(&[
            "insufficient credit",
            "insufficient balance",
            "insufficient quota",
            "dumate_quota_exhausted",
            "余额不足",
            "积分不足",
            "积分耗尽",
            "积分已用完",
            "额度耗尽",
        ])
    {
        "quota_limited"
    } else if status == 429
        || contains(&[
            "dumate_rate_limited",
            "rate_limit_error",
            "rate limit exceeded",
            "请求频率过高",
            "请求过于频繁",
        ])
    {
        "quota_limited"
    } else {
        "fatal"
    }
}

pub fn effective_status(status: u16, value: &Value) -> u16 {
    if (400..600).contains(&status) {
        return status;
    }
    match kind(status, value) {
        "token_expired" => 401,
        "quota_limited" => {
            let text = format!(
                "{} {} {}",
                message(value),
                util::text(&value["error"], &["type", "code"]),
                util::text(value, &["type", "code"])
            )
            .to_ascii_lowercase();
            if text.contains("rate") || text.contains("频率") || text.contains("频繁") {
                429
            } else {
                402
            }
        }
        "content_blocked" => 403,
        _ => match util::text(&value["error"], &["type"]).as_str() {
            "invalid_request_error" => 400,
            "permission_error" => 403,
            _ => code(value)
                .filter(|code| (400..600).contains(code))
                .map(|code| code as u16)
                .unwrap_or(502),
        },
    }
}

pub fn upstream(status: u16, value: &Value, protected: &[String]) -> Error {
    let status = effective_status(status, value);
    let mut error = Error::http(
        status,
        format!(
            "百度搭子返回 {status}：{}",
            util::safe_text(&message(value), protected, 600)
        ),
    )
    .with_code(match kind(status, value) {
        "token_expired" => "dumate_session_expired",
        "content_blocked" => "dumate_content_blocked",
        "quota_limited" if status == 429 => "dumate_rate_limited",
        "quota_limited" => "dumate_quota_exhausted",
        _ => "dumate_upstream",
    });
    error.upstream_code = code(value);
    error
}

pub fn classify(status: u16, body: &Value) -> Value {
    let body = body
        .as_str()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .unwrap_or_else(|| body.clone());
    let status = effective_status(status, &body);
    let reset_at = util::timestamp(&body["resetAt"])
        .or_else(|| util::timestamp(&body["error"]["resetAt"]))
        .filter(|at| *at > agent2api_wasm_sdk::now());
    json!({ "kind": kind(status, &body), "status": status,
        "message": format!("百度搭子返回 {status}：{}", message(&body).chars().take(1000).collect::<String>()),
        "upstream_code": code(&body), "reset_at": reset_at })
}

pub fn identity() -> Error {
    Error::http(
        409,
        "百度搭子 Cookie 对应的账号与原记录不一致，请单独添加新账号",
    )
    .with_code("dumate_identity_mismatch")
}

pub fn changed() -> Error {
    Error::http(409, "百度搭子账号已变更或停用，本次操作已停止").with_code("dumate_account_changed")
}

pub fn stopped(value: &Value) -> bool {
    let value = value
        .as_str()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or_else(|| value.clone());
    [&value, &value["error"]].iter().any(|value| {
        matches!(
            value["code"].as_str(),
            Some(
                "dumate_stream_incomplete"
                    | "dumate_account_changed"
                    | "dumate_identity_mismatch"
                    | "image_output_uncertain"
            )
        )
    })
}

pub fn event(error: &Error) -> Value {
    json!({ "error": { "message": error.message, "status": error.status.unwrap_or(502),
        "code": error.code, "type": "upstream_error", "resetAt": error.reset_at } })
}
