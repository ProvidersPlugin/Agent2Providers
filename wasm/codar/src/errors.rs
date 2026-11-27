use agent2api_wasm_sdk::{json, Error, Value};

use crate::util;

pub fn code(value: &Value) -> String {
    let objects = [
        value,
        &value["detail"],
        &value["error"],
        &value["response"]["error"],
    ];
    for object in objects {
        let code = util::text(object, &["providerCode", "code"]);
        if !code.is_empty() {
            return code;
        }
    }
    for object in objects {
        let kind = util::text(object, &["type"]);
        if !kind.is_empty() && !matches!(kind.as_str(), "error" | "response.failed") {
            return kind;
        }
    }
    String::new()
}

pub fn message(value: &Value) -> String {
    for object in [&value["error"], &value["detail"], value, &value["data"]] {
        let message = util::text(object, &["message", "msg", "error", "detail"]);
        if !message.is_empty() {
            return message;
        }
    }
    if let Some(message) = value["detail"].as_str() {
        return message.to_string();
    }
    "上游请求失败".into()
}

pub fn failed(value: &Value) -> bool {
    value["success"] == false
        || value["type"] == "error"
        || value["type"] == "response.failed"
        || value.get("error").is_some_and(|value| !value.is_null())
        || value["response"]
            .get("error")
            .is_some_and(|value| !value.is_null())
        || matches!(
            code(value).as_str(),
            "account_restricted" | "client_not_supported"
        )
        || util::number(&value["code"]).is_some_and(|code| code != 200.0)
}

fn kind(status: u16, value: &Value) -> &'static str {
    let code = code(value);
    let lower = format!("{code} {}", message(value)).to_ascii_lowercase();
    let contains = |words: &[&str]| words.iter().any(|word| lower.contains(word));
    if matches!(
        code.as_str(),
        "account_restricted"
            | "client_not_supported"
            | "codar_account_changed"
            | "codar_identity_mismatch"
            | "codar_image_uncertain"
    ) {
        "fatal"
    } else if status == 401
        || contains(&[
            "invalid_token",
            "invalid token",
            "token expired",
            "token_expired",
            "authentication_error",
            "codar_token_expired",
        ])
    {
        "token_expired"
    } else if status == 402
        || status == 429
        || contains(&[
            "quota_exceeded",
            "insufficient_quota",
            "insufficient balance",
            "余额不足",
            "积分不足",
            "rate_limit",
            "concurrent_limit",
        ])
    {
        "quota_limited"
    } else if contains(&[
        "content_filter",
        "content_policy",
        "content_blocked",
        "内容审核",
    ]) {
        "content_blocked"
    } else {
        "fatal"
    }
}

fn is_balance(status: u16, value: &Value) -> bool {
    let code = code(value);
    let lower = format!("{code} {}", message(value)).to_ascii_lowercase();
    if ["rate_limit", "rate limit", "concurrent_limit", "too many"]
        .iter()
        .any(|word| lower.contains(word))
    {
        return false;
    }
    status == 402
        || code == "codar_balance"
        || ["insufficient balance", "insufficient credit", "余额不足", "积分不足"]
            .iter()
            .any(|word| lower.contains(word))
}

pub fn upstream(status: u16, value: &Value, protected: &[String]) -> Error {
    let business = util::number(&value["code"])
        .filter(|code| code.fract() == 0.0 && (400.0..600.0).contains(code));
    let mut upstream = code(value);
    if upstream == "error" || upstream == "response.failed" {
        upstream = code(&value["error"]);
        if upstream.is_empty() {
            upstream = code(&value["response"]["error"]);
        }
    }
    let effective = if status < 400 {
        business.map(|code| code as u16).unwrap_or(status)
    } else {
        status
    };
    let class = kind(effective, value);
    let balance = class == "quota_limited" && is_balance(effective, value);
    let status = if balance {
        402
    } else if status >= 400 {
        status
    } else {
        business.map(|code| code as u16).unwrap_or(match class {
            "token_expired" => 401,
            "quota_limited" => 429,
            "content_blocked" => 403,
            _ => 502,
        })
    };
    if upstream.is_empty() || upstream.bytes().all(|byte| byte.is_ascii_digit()) {
        upstream = match class {
            "token_expired" => "codar_token_expired",
            "quota_limited" if balance => "codar_balance",
            "quota_limited" => "codar_quota_limited",
            "content_blocked" => "codar_content_blocked",
            _ => "codar_upstream",
        }
        .into();
    }
    let detail = util::safe_text(&message(value), protected, 400);
    let summary = if upstream == "client_not_supported" {
        format!(
            "科搭返回 {status}（当前登录凭证仅支持单轮无工具请求，不支持多轮或工具调用）：{detail}"
        )
    } else if detail.starts_with("科搭返回 ") {
        detail
    } else {
        format!("科搭返回 {status}：{detail}")
    };
    let mut error = Error::http(status, summary).with_code(upstream);
    error.upstream_code = business.map(|code| code as i64);
    for object in [
        value,
        &value["detail"],
        &value["error"],
        &value["response"]["error"],
    ] {
        if let Some(time) = ["resetAt", "reset_at", "retry_at"]
            .iter()
            .find_map(|key| util::timestamp(&object[*key]))
        {
            error.reset_at = Some(time);
            break;
        }
    }
    error
}

pub fn classify(status: u16, value: &Value) -> Value {
    let kind = kind(status, value);
    let status = if kind == "quota_limited" && is_balance(status, value) { 402 } else { status };
    let mut result = json!({ "kind": kind, "message": util::safe_text(&message(value), &[], 600), "status": status,
        "upstream_code": value["upstreamCode"].as_i64().or_else(|| value["code"].as_i64()) });
    if kind == "quota_limited" {
        result["reset_at"] = json!([
            value,
            &value["detail"],
            &value["error"],
            &value["response"]["error"]
        ]
        .iter()
        .find_map(|object| ["resetAt", "reset_at", "retry_at"]
            .iter()
            .find_map(|key| util::timestamp(&object[*key]))));
    }
    result
}

pub fn changed() -> Error {
    Error::http(409, "科搭账号、凭据或启用状态已变化，本次操作已停止")
        .with_code("codar_account_changed")
}
pub fn identity() -> Error {
    Error::http(409, "科搭账号身份与原记录不一致，请单独添加新账号")
        .with_code("codar_identity_mismatch")
}

pub fn event(error: &Error) -> Value {
    json!({ "error": { "message": error.message, "type": error.code.as_deref().unwrap_or("codar_upstream"),
        "code": error.upstream_code, "status": error.status, "resetAt": error.reset_at } })
}
