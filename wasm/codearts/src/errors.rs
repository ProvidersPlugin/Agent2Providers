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

pub fn upstream(status: u16, operation: &str) -> Error {
    if status == 401 {
        return Error::http(
            401,
            "CodeArts 凭据被上游拒绝，请从主程序续期入口更新；缺少原授权材料时需重新登录",
        )
        .with_code("codearts_credential_rejected");
    }
    let status = if (400..500).contains(&status) {
        status
    } else {
        502
    };
    Error::http(
        status,
        format!("CodeArts {operation}未获上游成功确认（HTTP {status}）；原始诊断已隐藏"),
    )
    .with_code("codearts_upstream")
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

pub fn classify(status: u16, body: &Value) -> Value {
    let payload = body
        .get("error")
        .filter(|error| error.is_object())
        .unwrap_or(body);
    let code = ["error_code", "providerCode", "code"]
        .iter()
        .find_map(|key| payload[*key].as_str())
        .unwrap_or("");
    let lower = payload["message"]
        .as_str()
        .or_else(|| payload["error_msg"].as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let image = code.contains("001001020")
        || code == "codearts_not_multimodal"
        || lower.contains("is not multimodal")
        || lower.contains("不支持图片输入");
    let token = code == "codearts_credential_rejected"
        || code == "codearts_access_expired"
        || code == "token_expired"
        || code == "APIG.0602"
        || status == 401
        || lower.contains("token expired")
        || lower.contains("invalid token");
    let blocked = !image
        && (code == "codearts_content_blocked"
            || code == "content_blocked"
            || lower.contains("content policy")
            || lower.contains("content blocked")
            || lower.contains("content filter")
            || lower.contains("内容策略拒绝"));
    let concurrency = code == "TM.00001041" || code == "codearts_concurrency_limited";
    let quota = !image
        && !blocked
        && !concurrency
        && (status == 429
            || code.contains("429")
            || code == "quota_limited"
            || code == "insufficient_quota"
            || lower.contains("insufficient quota")
            || lower.contains("rate limit")
            || lower.contains("too many")
            || lower.contains("额度"));
    let rejected_model = code == "model_not_found"
        || lower.contains("model not found")
        || lower.contains("invalid model");
    let status = if token {
        401
    } else if quota {
        429
    } else if image || rejected_model {
        400
    } else if blocked {
        403
    } else if concurrency {
        409
    } else if (400..600).contains(&status) {
        status
    } else {
        502
    };
    let reset_at = payload["resetAt"]
        .as_i64()
        .or_else(|| payload["reset_at"].as_i64())
        .filter(|at| {
            *at > 0
                && *at <= (1_i64 << 53) - 1
                && chrono::DateTime::from_timestamp_millis(*at).is_some()
        });
    let upstream_code = payload["upstreamCode"]
        .as_i64()
        .or_else(|| payload["upstream_code"].as_i64())
        .or_else(|| payload["code"].as_i64());
    json!({"kind":if token {"token_expired"} else if quota {"quota_limited"} else if blocked {"content_blocked"} else {"fatal"},
        "status":status,"reset_at":reset_at,"upstream_code":upstream_code,
        "message":if token {"CodeArts 登录凭据已失效，请续期或重新登录"} else if quota {"CodeArts 额度或速率窗口受限，请稍后重试"}
            else if concurrency {"CodeArts 当前账号已有三路对话，请等待在途请求结束，不会把并发占用记为额度耗尽"}
            else if image {"当前 CodeArts 模型不支持图片输入，请更换模型或去掉图片"}
            else if rejected_model {"当前 CodeArts 模型未获上游确认，请重新获取本账号目录"}
            else if blocked {"CodeArts 内容策略拒绝了本次请求"}
            else {"CodeArts 上游未完成本次请求，原始敏感诊断已隐藏"}})
}

pub fn classified(status: u16, body: &Value) -> Error {
    let class = classify(status, body);
    let status = class["status"]
        .as_u64()
        .and_then(|status| u16::try_from(status).ok())
        .unwrap_or(502);
    let code = match class["kind"].as_str() {
        Some("token_expired") => "token_expired",
        Some("quota_limited") => "quota_limited",
        Some("content_blocked") => "codearts_content_blocked",
        _ if status == 409
            && class["message"]
                .as_str()
                .is_some_and(|message| message.contains("已有三路")) =>
        {
            "codearts_concurrency_limited"
        }
        _ if status == 400
            && class["message"]
                .as_str()
                .is_some_and(|message| message.contains("不支持图片输入")) =>
        {
            "codearts_not_multimodal"
        }
        _ => "codearts_upstream",
    };
    let mut error = Error::http(
        status,
        class["message"]
            .as_str()
            .unwrap_or("CodeArts 本次请求未获确认"),
    )
    .with_code(code);
    error.reset_at = class["reset_at"].as_i64();
    error.upstream_code = class["upstream_code"].as_i64();
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
