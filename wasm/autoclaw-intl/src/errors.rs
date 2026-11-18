//! 四档分类：鉴权拒绝、已确认额度码/HTTP 429、内容策略、其余 fatal。
//! 国际版已复现 810000 免费额度和 810001 高负载；403 不能一律当登录失效。
//! 证据：magpie-community/plugins#367 的 0.2.22 双架构回执与 #924。
use super::{prompt, util};
use agent2api_wasm_sdk::{json, Error, Value};

pub fn code(body: &Value) -> Option<i64> {
    [
        body.get("upstream_code"),
        body.get("upstreamCode"),
        body.get("code"),
        body.pointer("/error/code"),
        body.pointer("/error/upstreamCode"),
        body.pointer("/error/upstream_code"),
    ]
    .into_iter()
    .flatten()
    .find_map(util::integer)
}
fn message(body: &Value) -> String {
    let message = util::text(body, &["message", "msg"]);
    if !message.is_empty() {
        return message;
    }
    let nested = util::text(&body["error"], &["message", "msg"]);
    if nested.is_empty() {
        body.as_str().unwrap_or("").to_string()
    } else {
        nested
    }
}
pub fn has_error(body: &Value) -> bool {
    body.get("error").is_some_and(|value| !value.is_null())
        || body["type"] == "error"
        || code(body).is_some_and(|code| code != 0)
}
pub fn classify(status: u16, body: &Value) -> Value {
    let raw = message(body).to_ascii_lowercase();
    let failure = if (400..600).contains(&status) {
        status
    } else {
        502
    };
    let upstream = code(body);
    let (kind, status, hint) = if upstream == Some(410_004) {
        (
            "fatal",
            403,
            "AutoClaw 国际版账号被上游停用或封禁，重新登录不能解除，请联系官方处理".to_string(),
        )
    } else if upstream == Some(810_000) {
        (
            "quota_limited",
            429,
            "AutoClaw 国际版当前模型的免费额度已用完；其他模型可能仍可使用，请检查官方额度或套餐"
                .to_string(),
        )
    } else if upstream == Some(810_001) {
        (
            "quota_limited",
            429,
            "AutoClaw 国际版当前模型高负载或服务配额受限，请稍后重试；不会当作登录失效".to_string(),
        )
    } else if status == 401 || upstream.is_some_and(super::client::auth_code) {
        (
            "token_expired",
            401,
            "AutoClaw 凭据被上游拒绝，请续期或重新登录".to_string(),
        )
    } else if status == 429 {
        (
            "quota_limited",
            429,
            "AutoClaw 上游额度或频率受限，请稍后重试".to_string(),
        )
    } else if body["code"] == "content_filter"
        || body["error"]["code"] == "content_filter"
        || body["providerCode"] == "content_filter"
        || status >= 400
            && [
                "blocked by security policy",
                "unapproved channel",
                "illegal api invocation",
            ]
            .iter()
            .any(|pattern| raw.contains(*pattern))
    {
        (
            "content_blocked",
            failure,
            "AutoClaw 上游内容策略拒绝，不是账号额度问题；请调整系统提示或对话内容".to_string(),
        )
    } else if status == 406 && raw.is_empty() {
        (
            "fatal",
            406,
            format!("AutoClaw 上游拒收请求{}", prompt::UPSTREAM_406_HINT),
        )
    } else {
        (
            "fatal",
            failure,
            format!("AutoClaw 上游请求失败（HTTP {status}）"),
        )
    };
    json!({ "kind": kind, "status": status, "message": hint, "upstream_code": code(body), "reset_at": null })
}
pub fn failure(status: u16, body: &Value) -> Error {
    let class = classify(status, body);
    let mut error = Error::http(
        class["status"].as_u64().unwrap_or(502) as u16,
        class["message"].as_str().unwrap_or("AutoClaw 上游请求失败"),
    );
    error.upstream_code = code(body);
    error.code = Some(if class["kind"] == "content_blocked" {
        "content_filter".into()
    } else {
        error
            .upstream_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "upstream_error".into())
    });
    error
}
pub fn event(error: &Error) -> Value {
    json!({ "error": { "message": error.message, "status": error.status.unwrap_or(502), "code": error.code,
        "upstreamCode": error.upstream_code, "type": if error.status == Some(401) { "authentication_error" }
            else if error.status == Some(429) { "rate_limit_error" } else { "upstream_error" } } })
}
