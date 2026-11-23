//! 区分HTTP状态、errno与status.code；缺码不当成功。流前/流后均保留真实错误。
//! 上游正文可能含Cookie/三件套/活动token，公开错误绝不回显URL或凭据。

use crate::util;
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn message(value: &Value) -> String {
    let own = util::text(value, &["show_msg", "errmsg", "message", "msg"]);
    if !own.is_empty() {
        return own;
    }
    let status = util::text(&value["status"], &["msg", "message"]);
    if !status.is_empty() {
        return status;
    }
    util::text(&value["error"], &["message", "msg"])
}
fn kind(status: u16, code: Option<i64>, message: &str) -> (&'static str, u16) {
    let lower = message.to_ascii_lowercase();
    if matches!(code, Some(-6 | 1000004))
        || matches!(status, 401 | 403)
        || [
            "not login",
            "not_login",
            "bduss invalid",
            "登录态已失效",
            "未登录",
        ]
        .iter()
        .any(|s| lower.contains(s))
    {
        return ("token_expired", 401);
    }
    if code == Some(11002)
        || status == 429
        || [
            "insufficient_balance",
            "积分不足",
            "额度不足",
            "quota",
            "rate limit",
            "限流",
            "频率过高",
        ]
        .iter()
        .any(|s| lower.contains(s))
    {
        return ("quota_limited", 429);
    }
    if [
        "content_filter",
        "content blocked",
        "内容违规",
        "敏感内容",
        "内容安全",
    ]
    .iter()
    .any(|s| lower.contains(s))
    {
        return ("content_blocked", 400);
    }
    (
        "fatal",
        if (400..600).contains(&status) {
            status
        } else {
            502
        },
    )
}
pub fn upstream(status: u16, code: Option<i64>, value: &Value, secrets: &[&str]) -> Error {
    let message = message(value);
    let (kind, status) = kind(status, code, &message);
    let safe = safe_message(&message, secrets);
    let mut error = Error::http(
        status,
        if safe.is_empty() {
            format!("库库请求失败（HTTP {status}）")
        } else {
            safe
        },
    )
    .with_code(match kind {
        "token_expired" => "kuku_auth",
        "quota_limited" => "kuku_quota",
        "content_blocked" => "kuku_content_blocked",
        _ => "kuku_upstream",
    });
    error.upstream_code = code;
    error
}
pub fn errno(value: &Value, secrets: &[&str]) -> Result<()> {
    let code = util::integer(&value["errno"]);
    match code {
        Some(0) => Ok(()),
        Some(code) => Err(upstream(502, Some(code), value, secrets)),
        None => Err(Error::http(502, "库库响应缺少errno，不能确认成功").with_code("kuku_protocol")),
    }
}
pub fn status(value: &Value, secrets: &[&str]) -> Result<()> {
    let code = util::integer(&value["status"]["code"]);
    match code {
        Some(0) => Ok(()),
        Some(code) => Err(upstream(502, Some(code), value, secrets)),
        None => {
            Err(Error::http(502, "库库响应缺少业务状态，不能确认成功").with_code("kuku_protocol"))
        }
    }
}
pub fn classify(status: u16, value: &Value) -> Value {
    let text = message(value);
    let code = util::integer(&value["upstream_code"])
        .or_else(|| util::integer(&value["error"]["upstream_code"]))
        .or_else(|| util::integer(&value["errno"]))
        .or_else(|| util::integer(&value["data"]["code"]))
        .or_else(|| util::integer(&value["data"]["errno"]))
        .or_else(|| util::integer(&value["status"]["code"]));
    let (kind, status) = if value["code"] == "kuku_content_blocked"
        || value["error"]["code"] == "kuku_content_blocked"
    {
        ("content_blocked", 400)
    } else {
        kind(status, code, &text)
    };
    json!({ "kind": kind, "status": status, "message": safe_message(&text, &[]), "upstream_code": code, "reset_at": null })
}
pub fn uncertain(error: Error) -> Error {
    if host_stop(&error) || matches!(error.status, Some(400 | 401 | 403 | 429)) {
        return error;
    }
    Error {
        message: format!(
            "{}；上游可能已创建任务，请先查看任务与积分，不会自动重复发送",
            error.message
        ),
        code: Some("kuku_turn_uncertain".into()),
        ..error
    }
}

pub fn stop(value: &Value) -> bool {
    [value, &value["error"]].iter().any(|value| {
        value["code"].as_str().is_some_and(|code| {
            matches!(
                code,
                "kuku_turn_uncertain" | "kuku_requires_client" | "kuku_stream_incomplete"
            )
        })
    })
}

pub fn is_error_response(status: u16, value: &Value) -> bool {
    status >= 400
        || value.get("error").is_some_and(|error| !error.is_null())
        || [
            util::integer(&value["errno"]),
            util::integer(&value["status"]["code"]),
        ]
        .into_iter()
        .flatten()
        .any(|code| code != 0)
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
        )
    )
}
pub fn transport(error: Error) -> Error {
    if host_stop(&error) {
        return error;
    }
    if error.code.as_deref() == Some("timeout") {
        Error::http(504, "库库请求超时，请检查网络或账号代理").with_code("kuku_transport")
    } else {
        Error::http(502, "无法连接库库，请检查网络或账号代理").with_code("kuku_transport")
    }
}
pub fn redact(input: &str, secrets: &[&str]) -> String {
    let mut text = input.to_string();
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        for needle in [secret.to_string(), util::query(secret)] {
            text = text.replace(&needle, "[已隐藏]");
        }
    }
    text
}
pub fn safe_message(input: &str, secrets: &[&str]) -> String {
    let text = redact(input, secrets);
    let lower = text.to_ascii_lowercase();
    if [
        "http://",
        "https://",
        "bduss=",
        "stoken=",
        "ptoken=",
        "jstoken=",
        "bdstoken=",
        "uinfo=",
        "cookie:",
    ]
    .iter()
    .any(|hint| lower.contains(hint))
    {
        return "库库返回了含敏感会话信息的错误，详细内容已隐藏".into();
    }
    util::safe_text(&text, 400)
}
pub fn sanitize(value: &Value, secrets: &[&str]) -> Value {
    match value {
        Value::String(text) => json!(redact(text, secrets)),
        Value::Array(items) => {
            Value::Array(items.iter().map(|value| sanitize(value, secrets)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| {
                    let key = key.to_ascii_lowercase().replace(['_', '-'], "");
                    ![
                        "cookie",
                        "cookies",
                        "accesstoken",
                        "bduss",
                        "stoken",
                        "ptoken",
                        "ptokenbfess",
                        "bdstoken",
                        "uinfo",
                        "jstoken",
                        "authorization",
                    ]
                    .contains(&key.as_str())
                })
                .map(|(key, value)| (key.clone(), sanitize(value, secrets)))
                .collect(),
        ),
        _ => value.clone(),
    }
}
pub fn event(error: &Error) -> Value {
    json!({ "error": { "message": error.message, "code": error.code, "status": error.status,
        "upstream_code": error.upstream_code, "type": if error.status == Some(429) { "rate_limit_error" } else { "upstream_error" } } })
}
