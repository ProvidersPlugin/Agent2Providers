//! 所有边界错误使用固定文案，不回显请求 URL、上游正文或凭据。

use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn invalid(message: &str) -> Error {
    Error::http(400, message).with_code("codearts_invalid_input")
}
pub fn protocol(message: &str) -> Error {
    Error::http(502, message).with_code("codearts_protocol")
}
pub fn limited(message: &str) -> Error {
    Error::http(503, message).with_code("codearts_resource_limit")
}

pub fn host_stop(error: &Error) -> bool {
    matches!(
        error.code.as_deref(),
        Some(
            "aborted"
                | "forbidden"
                | "plugin_resource_limit"
                | "plugin_protocol_error"
                | "plugin_unavailable"
                | "plugin_trap"
                | "timeout"
                | "codearts_account_changed"
                | "codearts_account_disabled"
        )
    )
}

pub fn host(mut error: Error) -> Error {
    let (status, message) = match error.code.as_deref() {
        Some("aborted") => (499, "CodeArts 操作已取消，未把不完整结果记录为成功"),
        Some("forbidden") => (403, "CodeArts 当前账号或主程序能力未获授权"),
        Some("timeout") => (
            504,
            "CodeArts 请求总期限或外层调用预算已到，结果未确认；未自动重试",
        ),
        Some("plugin_resource_limit") => (503, "CodeArts 操作超过主程序资源上限"),
        Some("plugin_protocol_error" | "plugin_trap") => (502, "CodeArts 主程序接口返回异常"),
        _ => (503, "CodeArts 所需主程序能力暂不可用"),
    };
    error.message = message.to_string();
    error.status = Some(status);
    error.upstream_code = None;
    error.credential_scope = None;
    error.reset_at = None;
    if !matches!(
        error.code.as_deref(),
        Some(
            "aborted"
                | "forbidden"
                | "timeout"
                | "plugin_resource_limit"
                | "plugin_protocol_error"
                | "plugin_unavailable"
                | "plugin_trap"
        )
    ) {
        error.code = Some("codearts_host_unavailable".into());
    }
    error
}

pub fn transport(error: Error) -> Error {
    if host_stop(&error) {
        return host(error);
    }
    Error::http(
        502,
        "CodeArts 请求或响应读取失败；没有在本次调用内切换账号、回退直连或重发请求",
    )
    .with_code("codearts_transport")
}

/// 解析上游错误响应体；非 JSON 文本原样保留以便展示上游原文。
pub fn parse_body(bytes: &[u8]) -> Value {
    if let Ok(value) = serde_json::from_slice::<Value>(bytes) {
        return value;
    }
    let text = String::from_utf8_lossy(bytes).trim().to_string();
    if text.is_empty() {
        Value::Null
    } else {
        Value::String(text)
    }
}

/// 遮蔽上游文本中可能回显的凭据，同时保留非敏感原文与 URL。
pub fn clean_upstream_text(text: &str, secrets: &[String], limit: usize) -> String {
    let mut masked = text.to_string();
    let mut ordered: Vec<&str> = secrets
        .iter()
        .map(String::as_str)
        .filter(|secret| !secret.is_empty())
        .collect();
    ordered.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    ordered.dedup();
    for secret in ordered {
        masked = masked.replace(secret, "[已隐藏]");
    }
    let mut cleaned = String::new();
    let mut count = 0usize;
    for ch in masked.chars() {
        let ch = if matches!(ch, '\r' | '\n' | '\t') {
            ' '
        } else if ch.is_control() {
            continue;
        } else {
            ch
        };
        if count >= limit {
            break;
        }
        cleaned.push(ch);
        count += 1;
    }
    cleaned.trim().to_string()
}

pub fn upstream(status: u16, _operation: &str, body: &Value, secrets: &[String]) -> Error {
    if status == 401 {
        return Error::http(401, format_upstream_message(401, body, secrets))
            .with_code("codearts_credential_rejected");
    }
    let resolved = if (400..600).contains(&status) {
        status
    } else {
        502
    };
    let message = format_upstream_message(status, body, secrets);
    Error::http(resolved, message).with_code("codearts_upstream")
}

pub fn spellings(secret: &str) -> Vec<String> {
    if secret.is_empty() {
        return Vec::new();
    }
    let mut values = vec![secret.to_string(), crate::encoding::form(secret)];
    if let Ok(value) = serde_json::to_string(secret) {
        values.push(value.trim_matches('"').to_string());
    }
    values.sort();
    values.dedup();
    values
}

pub fn contains_secret(text: &str, secrets: &[String]) -> bool {
    secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .any(|secret| text.contains(secret))
}

pub fn public_text(text: &str, secrets: &[String], limit: usize) -> String {
    if contains_secret(text, secrets) || text.contains("://") {
        return "[已隐藏]".to_string();
    }
    let cleaned: String = text
        .chars()
        .filter(|ch| !ch.is_control())
        .take(limit)
        .collect();
    if contains_secret(&cleaned, secrets) {
        "[已隐藏]".to_string()
    } else {
        cleaned
    }
}

pub fn upstream_text(value: &Value, key: &str, secrets: &[String], limit: usize) -> Result<String> {
    let Some(value) = value.get(key).filter(|value| !value.is_null()) else {
        return Ok(String::new());
    };
    let text = value
        .as_str()
        .ok_or_else(|| protocol("CodeArts 上游名称字段不是文本"))?
        .trim();
    if text.chars().count() > limit
        || text.chars().any(char::is_control)
        || contains_secret(text, secrets)
        || text.contains("://")
    {
        return Err(protocol(
            "CodeArts 上游字段包含受保护内容或无效文本，未公开原文",
        ));
    }
    Ok(text.to_string())
}

struct Classified {
    kind: &'static str,
    status: u16,
    message: String,
    code: &'static str,
    reset_at: Option<i64>,
    upstream_code: Option<i64>,
}

/// 复合业务码末段的真实状态码：`InferHub.001001005.400` → 400。
/// `TM.00001041`、`APIG.0301` 的末段不在 4xx/5xx 内，不会被误当成状态码。
fn code_status(code: &str) -> Option<u16> {
    let tail = code.trim().rsplit('.').next()?;
    let status = tail.parse::<u16>().ok()?;
    (400..=599).contains(&status).then_some(status)
}

fn detail_from(body: &Value, payload: &Value, secrets: &[String]) -> String {
    let text = payload["message"]
        .as_str()
        .or_else(|| payload["error_msg"].as_str())
        .or_else(|| payload["msg"].as_str())
        .or_else(|| payload["error_message"].as_str())
        .or_else(|| payload["Message"].as_str())
        .or_else(|| payload["detail"].as_str())
        .or_else(|| payload["reason"].as_str())
        .or_else(|| body.get("error").and_then(Value::as_str))
        .unwrap_or("");
    let main = clean_upstream_text(text, secrets, 2000);
    let extras: Vec<String> = payload
        .get("details")
        .or_else(|| body.get("details"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let item_msg = item["error_msg"]
                .as_str()
                .or_else(|| item["message"].as_str())
                .or_else(|| item["msg"].as_str())
                .map(|value| clean_upstream_text(value, secrets, 400))
                .unwrap_or_default();
            let item_code = item["error_code"]
                .as_str()
                .or_else(|| item["code"].as_str())
                .map(|value| clean_upstream_text(value, secrets, 96))
                .unwrap_or_default();
            match (item_code.is_empty(), item_msg.is_empty()) {
                (true, true) => None,
                (false, true) => Some(item_code),
                (true, false) => Some(item_msg),
                (false, false) if item_code == item_msg => Some(item_msg),
                (false, false) => Some(format!("{item_code}: {item_msg}")),
            }
        })
        .filter(|item| !item.is_empty() && !main.contains(item))
        .collect();
    if main.is_empty() {
        extras.join("; ")
    } else if extras.is_empty() {
        main
    } else {
        format!("{main} ({})", extras.join("; "))
    }
}

/// 上游业务码：字符串或数字都接受，成功码不计入错误。
fn code_of(payload: &Value) -> String {
    ["error_code", "providerCode", "code"]
        .iter()
        .find_map(|key| {
            payload[*key]
                .as_str()
                .map(str::to_string)
                .or_else(|| payload[*key].as_i64().map(|code| code.to_string()))
        })
        .filter(|code| !matches!(code.trim(), "" | "0" | "0000"))
        .unwrap_or_default()
}

fn format_upstream_message(status: u16, body: &Value, secrets: &[String]) -> String {
    let prefix = if (400..600).contains(&status) {
        format!("上游返回 {status}")
    } else {
        "上游返回".to_string()
    };
    if let Some(raw) = body.as_str() {
        let cleaned = clean_upstream_text(raw, secrets, 2000);
        return if cleaned.is_empty() {
            format!("上游返回 HTTP {status}")
        } else {
            format!("{prefix}：{cleaned}")
        };
    }
    let payload = body
        .get("error")
        .filter(|error| error.is_object())
        .unwrap_or(body);
    let code = clean_upstream_text(&code_of(payload), secrets, 96);
    let detail = detail_from(body, payload, secrets);
    match (code.is_empty(), detail.is_empty()) {
        (false, false) if detail != code => format!("{prefix}（{code}）：{detail}"),
        (true, false) | (false, false) => format!("{prefix}：{detail}"),
        (false, true) => format!("{prefix}（{code}）"),
        (true, true) => {
            if body.is_null() {
                format!("上游返回 HTTP {status}")
            } else {
                let raw = clean_upstream_text(&body.to_string(), secrets, 2000);
                if raw.is_empty() || raw == "{}" {
                    format!("上游返回 HTTP {status}")
                } else {
                    format!("{prefix}：{raw}")
                }
            }
        }
    }
}

fn parse_reset_at(payload: &Value) -> Option<i64> {
    payload["resetAt"]
        .as_i64()
        .or_else(|| payload["reset_at"].as_i64())
        .filter(|at| {
            *at > 0
                && *at < (1_i64 << 53)
                && chrono::DateTime::from_timestamp_millis(*at).is_some()
        })
}

fn parse_upstream_code(payload: &Value) -> Option<i64> {
    payload["upstreamCode"]
        .as_i64()
        .or_else(|| payload["upstream_code"].as_i64())
        .or_else(|| payload["code"].as_i64())
}

fn details(status: u16, body: &Value, secrets: &[String]) -> Classified {
    let payload = body
        .get("error")
        .filter(|error| error.is_object())
        .unwrap_or(body);
    // 插件自身返回的 codearts_* / quota_limited 错误经过宿主二次分类时直接保留原文，
    // 不再当成未识别的上游错误重复套壳。
    if let Some(provider_code) = payload["providerCode"]
        .as_str()
        .filter(|code| code.starts_with("codearts_") || *code == "quota_limited")
    {
        let (kind, resolved_status, code_tag) = match provider_code {
            "codearts_credential_rejected" => ("token_expired", 401, "codearts_credential_rejected"),
            "quota_limited" => ("quota_limited", 429, "quota_limited"),
            "codearts_content_blocked" => ("content_blocked", 403, "codearts_content_blocked"),
            "codearts_concurrency_limited" => (
                "fatal",
                if (400..600).contains(&status) { status } else { 409 },
                "codearts_concurrency_limited",
            ),
            "codearts_invalid_input" => (
                "fatal",
                if (400..600).contains(&status) { status } else { 400 },
                "codearts_invalid_input",
            ),
            _ => (
                "fatal",
                if (400..600).contains(&status) { status } else { 502 },
                "codearts_upstream",
            ),
        };
        let raw_message = payload["message"]
            .as_str()
            .or_else(|| payload["error_msg"].as_str())
            .unwrap_or("");
        let message = clean_upstream_text(raw_message, secrets, 2000);
        let message = if message.is_empty() {
            format!("上游返回 HTTP {resolved_status}")
        } else {
            message
        };
        return Classified {
            kind,
            status: resolved_status,
            message,
            code: code_tag,
            reset_at: parse_reset_at(payload),
            upstream_code: parse_upstream_code(payload),
        };
    }
    let code_owned = code_of(payload);
    let code = code_owned.as_str();
    let code_lower = code.trim().to_ascii_lowercase();
    let message = payload["message"]
        .as_str()
        .or_else(|| payload["error_msg"].as_str())
        .or_else(|| payload["msg"].as_str())
        .or_else(|| body.as_str())
        .unwrap_or("");
    let lower = message.to_ascii_lowercase();
    let image = code_lower.contains("001001020")
        || code_lower == "codearts_not_multimodal"
        || lower.contains("is not multimodal")
        || message.contains("不支持图片输入");
    let token = code_lower == "codearts_credential_rejected"
        || code_lower == "codearts_access_expired"
        || code_lower == "token_expired"
        || code_lower.starts_with("apig.0301")
        || code_lower.starts_with("apig.0302")
        || code_lower.starts_with("apig.0303")
        || code_lower.starts_with("apig.0307")
        || code_lower.starts_with("apig.0602")
        || code_lower.starts_with("sts5.1804")
        || code_lower.starts_with("sts5.1806")
        || code_lower == "invalid_grant"
        || status == 401
        || lower.contains("token expired")
        || lower.contains("invalid token")
        || lower.contains("security token has expired")
        || lower.contains("verify ak sk signature fail");
    let blocked = !image
        && (code_lower == "codearts_content_blocked"
            || code_lower == "content_blocked"
            || lower.contains("content policy")
            || lower.contains("content blocked")
            || lower.contains("content filter")
            || message.contains("内容策略拒绝"));
    let concurrency = code_lower.starts_with("tm.00001041")
        || code_lower == "codearts_concurrency_limited";
    // 华为网关的限流/繁忙码：与原生同一口径，归入额度窗口。
    let rate_code = code_lower.starts_with("modelarts.81101")
        || code_lower.starts_with("modelarts.81111")
        || code_lower.starts_with("modelarts.81114")
        || code_lower == "busyexception";
    let quota = !image
        && !blocked
        && !concurrency
        && (status == 429
            || code.contains("429")
            || rate_code
            || code_lower == "quota_limited"
            || code_lower == "insufficient_quota"
            || lower.contains("insufficient quota")
            || lower.contains("rate limit")
            || lower.contains("too many")
            || message.contains("额度"));
    let model_not_found = code_lower == "model_not_found"
        || lower.contains("model not found")
        || lower.contains("invalid model");
    // 请求形态问题：上游明确拒绝，重试与换账号都不会变。
    let parameter = !image
        && (code_lower.starts_with("inferhub.001001005")
            || lower.contains("request param is invalid"));
    let model_mismatch = code_lower.starts_with("inferhub.001001011")
        || lower.contains("not equals requestheader")
        || lower.contains("model not equals");
    let tool_name = code_lower.starts_with("inferhub.001001017")
        || (lower.contains("function name") && lower.contains("length over limit"));
    let channel = code_lower == "4000" || lower.contains("invalid channel, group not found");
    let busy = code_lower.starts_with("tm.00000001") || message.contains("网络繁忙");
    let (kind, resolved_status, code_tag) = if token {
        ("token_expired", 401, "codearts_credential_rejected")
    } else if quota {
        ("quota_limited", 429, "quota_limited")
    } else if concurrency {
        ("fatal", 409, "codearts_concurrency_limited")
    } else if image {
        ("fatal", 400, "codearts_not_multimodal")
    } else if model_not_found {
        ("fatal", 400, "codearts_model_rejected")
    } else if parameter {
        ("fatal", 400, "codearts_parameter_rejected")
    } else if model_mismatch {
        ("fatal", 400, "codearts_model_mismatch")
    } else if tool_name {
        ("fatal", 400, "codearts_tool_name_rejected")
    } else if channel {
        ("fatal", 400, "codearts_channel_unsupported")
    } else if blocked {
        ("content_blocked", 403, "codearts_content_blocked")
    } else if busy {
        ("fatal", 503, "codearts_busy")
    } else {
        let resolved_status = code_status(code)
            .or_else(|| (400..600).contains(&status).then_some(status))
            .unwrap_or(502);
        ("fatal", resolved_status, "codearts_upstream")
    };
    let text = format_upstream_message(status, body, secrets);
    Classified {
        kind,
        status: resolved_status,
        message: text,
        code: code_tag,
        reset_at: parse_reset_at(payload),
        upstream_code: parse_upstream_code(payload),
    }
}

pub fn classify(status: u16, body: &Value, secrets: &[String]) -> Value {
    let class = details(status, body, secrets);
    json!({"kind":class.kind,"status":class.status,"reset_at":class.reset_at,
        "upstream_code":class.upstream_code,"message":class.message})
}

pub fn classified(status: u16, body: &Value, secrets: &[String]) -> Error {
    let class = details(status, body, secrets);
    let mut error = Error::http(class.status, class.message).with_code(class.code);
    error.reset_at = class.reset_at;
    error.upstream_code = class.upstream_code;
    error
}

pub fn has_answer(value: &Value) -> bool {
    let content = |value: &Value| {
        value["content"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
            || value["content"].as_array().is_some_and(|parts| {
                parts
                    .iter()
                    .any(|part| part["text"].as_str().is_some_and(|text| !text.is_empty()))
            })
            || value["reasoning_content"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
            || value["tool_calls"]
                .as_array()
                .is_some_and(|calls| !calls.is_empty())
    };
    content(&value["delta"])
        || value["choices"].as_array().is_some_and(|choices| {
            choices.iter().any(|choice| {
                content(&choice["delta"])
                    || content(&choice["message"])
                    || choice["text"].as_str().is_some_and(|text| !text.is_empty())
            })
        })
        || value["text"]
            .as_str()
            .is_some_and(|text| !text.trim().is_empty() && text.trim() != "[DONE]")
}

pub fn failure(value: &Value) -> bool {
    if !value.is_object() || has_answer(value) {
        return false;
    }
    value
        .get("error")
        .is_some_and(|error| !error.is_null() && error != false)
        || ["error_code", "code"].iter().any(|key| {
            value.get(*key).is_some_and(|code| {
                code.as_i64().is_some_and(|code| code != 0)
                    || code.as_str().is_some_and(|code| {
                        let code = code.trim().to_ascii_lowercase();
                        !code.is_empty() && !matches!(code.as_str(), "0" | "0000" | "success")
                    })
            })
        })
}
