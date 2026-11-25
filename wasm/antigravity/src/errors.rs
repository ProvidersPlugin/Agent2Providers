//! 只判上游错误对象，不扫描模型回答。四档分类/10秒瞬时退避沿用原生 adapter.rs。
//! 对外不透传原始 Google 响应或 URL，避免错误正文回显授权码、令牌或 state。

use super::util;
use agent2api_wasm_sdk::{json, Error, Value};

pub fn inner(value: &Value) -> &Value {
    value
        .get("response")
        .filter(|v| v.is_object())
        .unwrap_or(value)
}
fn object(value: &Value) -> &Value {
    if let Some(error) = value.get("error").filter(|v| v.is_object()) {
        return error;
    }
    let value = inner(value);
    value
        .get("error")
        .filter(|v| v.is_object())
        .unwrap_or(value)
}
fn marker(value: &Value) -> String {
    [
        value.get("providerCode"),
        value.get("code"),
        object(value).get("code"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .find(|v| {
        v.starts_with("antigravity_")
            || matches!(
                *v,
                "credential_conflict"
                    | "local_precondition"
                    | "plugin_resource_limit"
                    | "aborted"
                    | "image_output_uncertain"
            )
    })
    .unwrap_or("")
    .to_string()
}
fn text(value: &Value) -> String {
    let error = object(value);
    format!(
        "{} {}",
        util::text(error, &["status"]),
        util::text(error, &["message"])
    )
    .to_ascii_lowercase()
}
pub fn failed(value: &Value) -> bool {
    [value.get("error"), inner(value).get("error")]
        .into_iter()
        .flatten()
        .any(|v| !v.is_null() && *v != false && v.as_str() != Some(""))
}
pub fn numeric_code(value: &Value) -> Option<i64> {
    [
        object(value).get("code"),
        value.get("upstream_code"),
        value.get("upstreamCode"),
    ]
    .into_iter()
    .flatten()
    .find_map(util::integer)
}

pub fn content_blocked(value: &Value) -> bool {
    let inner = inner(value);
    if inner
        .pointer("/promptFeedback/blockReason")
        .and_then(Value::as_str)
        .is_some_and(|v| !v.is_empty() && v != "BLOCK_REASON_UNSPECIFIED")
    {
        return true;
    }
    if inner
        .get("candidates")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|c| {
            matches!(
                c["finishReason"].as_str(),
                Some(
                    "SAFETY"
                        | "RECITATION"
                        | "BLOCKLIST"
                        | "PROHIBITED_CONTENT"
                        | "SPII"
                        | "IMAGE_SAFETY"
                )
            )
        })
    {
        return true;
    }
    let message = text(value);
    [
        "content_filter",
        "content_policy",
        "safety policy",
        "blocked by safety",
        "prohibited_content",
        "blocked by content",
        "blocked by security policy",
        "unapproved channel",
        "illegal api invocation",
    ]
    .iter()
    .any(|term| message.contains(term))
}

fn duration(value: &Value) -> Option<u64> {
    let seconds = if let Some(text) = value.as_str() {
        let mut number = String::new();
        let mut total = 0.0f64;
        let mut seen = false;
        for byte in text.trim().bytes() {
            if byte.is_ascii_digit() || byte == b'.' {
                number.push(char::from(byte));
                continue;
            }
            let scale = match byte {
                b'h' => 3600.0,
                b'm' => 60.0,
                b's' => 1.0,
                _ => return None,
            };
            let amount = number.parse::<f64>().ok()?;
            if !amount.is_finite() || amount < 0.0 {
                return None;
            }
            total += amount * scale;
            number.clear();
            seen = true;
        }
        if !number.is_empty() || !seen {
            return None;
        }
        total
    } else {
        let seconds = util::integer(&value["seconds"])? as f64;
        seconds + value["nanos"].as_f64().unwrap_or(0.0) / 1_000_000_000.0
    };
    (seconds.is_finite() && seconds > 0.0 && seconds <= 7.0 * 86400.0)
        .then(|| (seconds * 1000.0).ceil() as u64)
}
pub fn retry_delay(value: &Value) -> Option<u64> {
    object(value)["details"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|detail| {
            let retry = detail["@type"]
                .as_str()
                .is_some_and(|kind| kind.ends_with("google.rpc.RetryInfo"))
                .then(|| duration(&detail["retryDelay"]))
                .flatten();
            let quota = detail["@type"]
                .as_str()
                .is_some_and(|kind| kind.ends_with("google.rpc.ErrorInfo"))
                .then(|| duration(&detail["metadata"]["quotaResetDelay"]))
                .flatten();
            retry.into_iter().chain(quota).max()
        })
        .max()
}
fn reset(value: &Value) -> Option<i64> {
    [
        value.get("reset_at"),
        value.get("resetAt"),
        object(value).get("reset_at"),
        object(value).get("resetAt"),
    ]
    .into_iter()
    .flatten()
    .filter_map(util::integer)
    .filter(|at| {
        *at > 0
            && *at <= (1_i64 << 53) - 1
            && chrono::DateTime::from_timestamp_millis(*at).is_some()
    })
    .chain(
        retry_delay(value)
            .and_then(|delay| Some(util::now().ok()?.saturating_add(i64::try_from(delay).ok()?))),
    )
    .max()
}

pub fn upstream(status: u16, value: &Value) -> Error {
    let own = marker(value);
    let message = text(value);
    let code = numeric_code(value);
    let semantic = util::text(object(value), &["status"]);
    let permanent = object(value)["details"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|detail| {
            matches!(
                detail["reason"].as_str(),
                Some("TOS_VIOLATION" | "VALIDATION_REQUIRED")
            )
        });
    let projected = code
        .and_then(|c| u16::try_from(c).ok())
        .filter(|c| (400..=599).contains(c));
    let status = if status >= 400 {
        status
    } else {
        projected.unwrap_or(match semantic.as_str() {
            "RESOURCE_EXHAUSTED" => 429,
            "UNAUTHENTICATED" => 401,
            "PERMISSION_DENIED" => 403,
            "NOT_FOUND" => 404,
            "UNAVAILABLE" => 503,
            "INVALID_ARGUMENT" => 400,
            _ => 502,
        })
    };
    let (status, tag, detail) = if matches!(
        own.as_str(),
        "credential_conflict"
            | "local_precondition"
            | "plugin_resource_limit"
            | "aborted"
            | "image_output_uncertain"
    ) {
        (
            status,
            own.as_str(),
            "Antigravity 当前操作缺少必要条件或超过资源限制",
        )
    } else if own == "antigravity_content_blocked" || content_blocked(value) {
        (
            400,
            "antigravity_content_blocked",
            "Antigravity 内容策略拒绝了本次请求",
        )
    } else if own == "antigravity_quota" || message.contains("resource_exhausted") || status == 429
    {
        (
            429,
            "antigravity_quota",
            "Antigravity 当前模型额度或请求窗口受限，请等待重置",
        )
    } else if own == "antigravity_credentials" || status == 401 {
        (
            401,
            "antigravity_credentials",
            "Antigravity 登录凭据已失效，请续期或重新授权",
        )
    } else if own == "antigravity_account_action" || permanent {
        (
            403,
            "antigravity_account_action",
            "Antigravity 要求官方账号验证或已限制服务，请在官方页面处理；自动续期无法解除",
        )
    } else if own == "antigravity_eligibility" || status == 403 {
        (
            403,
            "antigravity_eligibility",
            "Antigravity 拒绝了账号资格、订阅或验证状态，请在官方页面确认",
        )
    } else if own == "antigravity_transient"
        || [
            "unavailable",
            "internal",
            "deadline_exceeded",
            "overloaded",
            "try again",
            "timeout",
        ]
        .iter()
        .any(|term| message.contains(term))
    {
        (
            status,
            "antigravity_transient",
            "Antigravity 上游暂时不可用，请稍后重试",
        )
    } else {
        (
            status,
            "antigravity_upstream",
            "Antigravity 上游未接受本次请求",
        )
    };
    let mut error = Error::http(status.min(599), detail).with_code(tag);
    error.upstream_code = code;
    if tag == "antigravity_quota" {
        error.reset_at = reset(value);
    }
    if matches!(tag, "antigravity_credentials" | "antigravity_eligibility") {
        error.credential_scope = Some("account".into());
    }
    error
}

pub fn classify(status: u16, value: &Value) -> Value {
    let error = upstream(status, value);
    let kind = match error.code.as_deref() {
        Some("antigravity_quota") => "quota_limited",
        Some("antigravity_credentials" | "antigravity_eligibility") => "token_expired",
        Some("antigravity_content_blocked") => "content_blocked",
        _ => "fatal",
    };
    json!({ "kind": kind, "status": error.status, "message": error.message,
        "upstream_code": error.upstream_code, "reset_at": error.reset_at })
}

pub fn advice(params: &Value) -> Value {
    if params["attempt"].as_u64().unwrap_or(0) >= params["budget"].as_u64().unwrap_or(0) {
        return Value::Null;
    }
    let body = &params["body"];
    let error = upstream(params["status"].as_u64().unwrap_or(502) as u16, body);
    let delay = if error.code.as_deref() == Some("antigravity_transient") {
        Some(10_000)
    } else if error.code.as_deref() == Some("antigravity_quota") {
        reset(body)
            .and_then(|at| {
                let remaining = at.saturating_sub(util::now().ok()?);
                (remaining > 0).then_some(remaining as u64)
            })
            .or_else(|| {
                body["retryAfterSecs"]
                    .as_u64()
                    .filter(|s| *s > 0 && *s <= 7 * 86400)
                    .map(|s| s.saturating_mul(1000))
            })
    } else {
        None
    };
    delay
        .map(|delay| json!({ "delay_ms": delay, "reason": "Antigravity 要求等待后重试" }))
        .unwrap_or(Value::Null)
}

pub fn retry(params: &Value) -> Value {
    let policy = upstream(
        params["status"].as_u64().unwrap_or(502) as u16,
        &params["body"],
    );
    if local(&policy) {
        return json!({ "action": "stop", "reason": "本次请求缺少必要条件，不能自动重发" });
    }
    let advice = advice(params);
    if advice.is_null() {
        json!({ "action": "default" })
    } else {
        json!({ "action": "retry", "delay_ms": advice["delay_ms"], "reason": advice["reason"] })
    }
}

pub fn transport(error: Error) -> Error {
    match error.code.as_deref() {
        Some("aborted") => Error::http(499, "Antigravity 请求已取消").with_code("aborted"),
        Some("forbidden") => {
            Error::http(403, "Antigravity 出网未获主程序授权").with_code("local_precondition")
        }
        Some("plugin_resource_limit") => Error::http(413, "Antigravity 请求超过主程序资源限制")
            .with_code("plugin_resource_limit"),
        Some("timeout") => {
            Error::http(504, "Antigravity 请求超时").with_code("antigravity_transient")
        }
        _ => Error::http(502, "Antigravity 网络请求失败，请检查已选择的出口代理")
            .with_code("antigravity_transport"),
    }
}

pub fn transport_failed(error: &Error) -> bool {
    error.code.as_deref() == Some("antigravity_transport")
        || (error.code.as_deref() == Some("antigravity_transient") && error.status == Some(504))
}

pub fn local(error: &Error) -> bool {
    matches!(
        error.code.as_deref(),
        Some(
            "credential_conflict"
                | "local_precondition"
                | "plugin_resource_limit"
                | "aborted"
                | "image_output_uncertain"
        )
    )
}
pub fn stream(error: &Error) -> Value {
    json!({ "error": { "message": error.message, "code": error.code,
        "status": error.status.unwrap_or(502), "upstream_code": error.upstream_code,
        "reset_at": error.reset_at, "type": "upstream_error" } })
}
