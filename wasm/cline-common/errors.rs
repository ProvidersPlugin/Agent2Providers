//! Cline 原生分类：401刷新，429按Try again in冷却，403权限错误与402保持Fatal。
//! 不能因免费账号credit为0/负数就判定免费模型不可用。

use agent2api_wasm_sdk::{self as sdk, json, Error, Value};

fn detail(value: &Value) -> String {
    [
        &value["message"],
        &value["error"]["message"],
        &value["error"],
        &value["msg"],
    ]
    .into_iter()
    .find_map(Value::as_str)
    .unwrap_or("")
    .to_string()
}

fn reset(body: &Value) -> Option<i64> {
    if let Some(at) = body["resetAt"]
        .as_i64()
        .or_else(|| body["reset_at"].as_i64())
        .filter(|value| *value > 0)
    {
        return Some(at);
    }
    let text = body.to_string();
    let marker = "Try again in";
    let at = text.find(marker)?;
    let rest = &text[at + marker.len()..];
    let end = rest.find(['"', '\n', '\r', '}']).unwrap_or(rest.len());
    let bytes = rest[..end].trim().as_bytes();
    let mut total = 0_i64;
    let mut number = 0_i64;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'0'..=b'9' => {
                number = number
                    .saturating_mul(10)
                    .saturating_add((bytes[index] - b'0') as i64)
            }
            b'd' => {
                total = total.saturating_add(number.saturating_mul(86_400_000));
                number = 0;
            }
            b'h' => {
                total = total.saturating_add(number.saturating_mul(3_600_000));
                number = 0;
            }
            b'm' if bytes.get(index + 1) == Some(&b's') => {
                total = total.saturating_add(number);
                number = 0;
                index += 1;
            }
            b'm' => {
                total = total.saturating_add(number.saturating_mul(60_000));
                number = 0;
            }
            b's' => {
                total = total.saturating_add(number.saturating_mul(1000));
                number = 0;
            }
            b' ' => {}
            _ => number = 0,
        }
        index += 1;
    }
    (total > 0).then(|| sdk::now().saturating_add(total))
}

pub fn stop_retry(body: &Value) -> bool {
    let code = body["providerCode"]
        .as_str()
        .or_else(|| body["code"].as_str())
        .or_else(|| body["error"]["code"].as_str())
        .unwrap_or("");
    matches!(
        code,
        "local_precondition"
            | "identity_changed"
            | "cline_refresh_source_changed"
            | "cline_refresh_source_unconfirmed"
            | "cline_refresh_save_unconfirmed"
            | "cline_refresh_invalid_response"
            | "cline_refresh_missing_field"
            | "cline_refresh_field_type"
            | "cline_refresh_empty_field"
            | "cline_refresh_alias_conflict"
            | "cline_refresh_identity_unconfirmed"
            | "cline_refresh_result_unconfirmed"
            | "cline_refresh_denied"
            | "cline_refresh_rejected"
            | "cline_refresh_incomplete"
            | "cline_free_refresh_requires_host"
            | "cline_free_refresh_unconfirmed"
            | "cline_desktop_relogin_required"
            | "cline_api_key_rejected"
    ) || body.to_string().contains("local_precondition")
}

pub fn classify(status: u16, body: &Value) -> Value {
    if stop_retry(body) {
        let message = detail(body);
        return json!({ "kind": "fatal", "status": status.clamp(400, 599),
            "message": if message.is_empty() { "Cline 当前操作不能自动重试" } else { &message },
            "reset_at": null, "upstream_code": null });
    }
    let text = detail(body).to_ascii_lowercase();
    let provider_code = body["providerCode"]
        .as_str()
        .or_else(|| body["code"].as_str())
        .or_else(|| body["error"]["code"].as_str())
        .unwrap_or("");
    let content = [
        "blocked by security policy",
        "unapproved channel",
        "illegal api invocation",
    ]
    .iter()
    .any(|marker| text.contains(marker))
        || matches!(provider_code, "cline_content" | "content_filter")
        || body["error"]["code"] == "content_filter";
    let (kind, code, message, reset) = if status == 401 {
        (
            "token_expired",
            401,
            "Cline 登录凭证已失效，请续期或重新登录",
            None,
        )
    } else if status == 429 {
        (
            "quota_limited",
            429,
            "Cline 当前模型额度窗口已达上限，请稍后或切换账号",
            reset(body),
        )
    } else if content {
        (
            "content_blocked",
            status.clamp(400, 599),
            "Cline 内容策略拒绝本次请求，请调整提示内容",
            None,
        )
    } else if status == 403 {
        (
            "fatal",
            403,
            "Cline 账号没有当前模型通道权限，请检查模型与客户端头配置",
            None,
        )
    } else if status == 402 {
        (
            "fatal",
            402,
            "Cline 当前请求需要付费 credit；没有改用其他额度池",
            None,
        )
    } else {
        (
            "fatal",
            status.clamp(400, 599),
            "Cline 上游未接受本次请求",
            None,
        )
    };
    json!({ "kind": kind, "status": code, "message": message, "reset_at": reset, "upstream_code": null })
}

pub fn upstream(status: u16, body: &Value) -> Error {
    let class = classify(status, body);
    let code = match class["kind"].as_str() {
        Some("token_expired") => "cline_token",
        Some("quota_limited") => "cline_quota",
        Some("content_blocked") => "cline_content",
        _ => "cline_upstream",
    };
    Error::http(
        class["status"].as_u64().unwrap_or(502) as u16,
        class["message"].as_str().unwrap_or("Cline 请求失败"),
    )
    .with_code(code)
    .with_reset(class["reset_at"].as_i64())
}

pub fn event(value: &Value) -> Option<Error> {
    if value
        .get("error")
        .is_some_and(|error| !error.is_null() && error.as_str() != Some(""))
        || value["type"] == "error"
        || value
            .get("success")
            .is_some_and(|success| success.as_bool() != Some(true))
    {
        let status = value["error"]["status"]
            .as_u64()
            .or_else(|| value["error"]["statusCode"].as_u64())
            .or_else(|| value["status"].as_u64())
            .or_else(|| value["statusCode"].as_u64())
            .or_else(|| value["error"]["code"].as_u64())
            .filter(|status| (400..600).contains(status))
            .unwrap_or(502) as u16;
        return Some(upstream(status, value));
    }
    None
}

pub fn stream_error(error: &Error) -> Value {
    json!({ "error": { "message": error.message, "status": error.status.unwrap_or(502), "code": error.code, "type": "upstream_error" } })
}
