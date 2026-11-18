//! HTTP 与业务码双校验；404/501 或 SaaS 未部署错误照实返回，不补钱包/套餐。
//! 官方 errcode.go 区分 401 未授权与 403 禁止；后者不是自动续期证据。
//! 不回显网络错误全文、响应体或 URL，避免 Cookie/模型 API Key 泄漏。

use agent2api_wasm_sdk::{json, Error, Value};

use crate::{endpoints::ReadEndpoint, util};

pub fn host_stop(error: &Error) -> bool {
    matches!(
        error.code.as_deref(),
        Some(
            "cancelled"
                | "canceled"
                | "timeout"
                | "deadline_exceeded"
                | "forbidden"
                | "permission_denied"
                | "plugin_unavailable"
                | "plugin_resource_limit"
                | "plugin_trap"
                | "plugin_protocol_error"
                | "plugin_cancelled"
                | "plugin_timeout"
                | "monkeycode_account_changed"
        )
    )
}

pub fn transport(error: Error, endpoint: ReadEndpoint) -> Error {
    // 保留宿主取消、权限和资源边界的码，不转成可重试的业务失败。
    let mut output = Error::http(
        error
            .status
            .filter(|status| (400..600).contains(status))
            .unwrap_or(502),
        format!(
            "MonkeyCode {}未完成（网络或主程序限制），未使用猜测值",
            endpoint.label()
        ),
    );
    output.code = error
        .code
        .or_else(|| Some("monkeycode_transport_error".into()));
    output
}

pub fn protocol(endpoint: ReadEndpoint, detail: &str) -> Error {
    Error::http(
        502,
        format!("MonkeyCode {}响应无效：{detail}", endpoint.label()),
    )
    .with_code("monkeycode_protocol_error")
}

pub fn upstream(status: u16, payload: &Value, endpoint: ReadEndpoint) -> Error {
    let upstream = util::integer(payload, "code");
    let (http, code, message) = if status == 401 || upstream == Some(401) {
        (
            401,
            "monkeycode_session_expired",
            "登录态未通过国内站校验，请重新登录后更新该账号的 monkeycode_ai_session；没有尝试自动登录、续期或其他地区账号".to_string(),
        )
    } else if status == 403 || upstream == Some(403) || matches!(upstream, Some(10001 | 10202)) {
        (
            403,
            "monkeycode_forbidden",
            "当前账号被拒绝访问，请确认账号权限；没有尝试续期或绕过限制".to_string(),
        )
    } else if matches!(status, 404 | 405 | 501) {
        (
            status,
            "monkeycode_endpoint_unavailable",
            format!(
                "{}返回 HTTP {status}，当前服务可能未部署此接口，未使用开源版默认值代替",
                endpoint.label()
            ),
        )
    } else if upstream == Some(10811) {
        (
            409,
            "monkeycode_task_busy",
            "任务名额已占用；未枚举或停止任何其他任务，请在官方页面自行处理".to_string(),
        )
    } else if status == 429 || upstream == Some(429) {
        (
            429,
            "monkeycode_rate_limited",
            "请求过于频繁，请稍后重试；未猜测重置时间".to_string(),
        )
    } else {
        (
            if (400..600).contains(&status) {
                status
            } else {
                502
            },
            "monkeycode_upstream_error",
            format!("{}失败，未生成替代数据", endpoint.label()),
        )
    };
    let mut error = Error::http(
        http,
        format!(
            "MonkeyCode {message}（上游 HTTP {status}，业务码 {}）",
            upstream
                .map(|code| code.to_string())
                .unwrap_or_else(|| "缺失".into())
        ),
    )
    .with_code(code);
    error.upstream_code = upstream;
    error
}

pub fn report(error: &Error) -> Value {
    json!({ "status": error.status, "code": error.code,
        "upstreamCode": error.upstream_code, "message": error.message })
}

pub fn replay_forbidden(body: &Value) -> bool {
    ["code", "providerCode", "provider_code"]
        .into_iter()
        .any(|key| {
            body[key].as_str().is_some_and(|code| {
                code.starts_with("monkeycode_task_")
                    || code.starts_with("monkeycode_remote_activity_")
                    || code == "unsupported_parameter"
                    || code == "monkeycode_conversation_bad_input"
            })
        })
}

pub fn classify(status: u16, body: &Value) -> Value {
    let upstream = body["upstreamCode"]
        .as_i64()
        .or_else(|| body["upstream_code"].as_i64())
        .or_else(|| util::integer(body, "code"));
    let blocked = body["code"]
        .as_str()
        .is_some_and(|code| code.ends_with("_not_connected"));
    let (kind, http, message) = if blocked {
        (
            "fatal",
            400,
            "MonkeyCode 当前输入不能由任务接口表达，请按提示调整",
        )
    } else if upstream == Some(10811) || body["code"] == "monkeycode_task_busy" {
        (
            "fatal",
            409,
            "MonkeyCode 任务名额已占用，未枚举或停止其他任务",
        )
    } else if status == 401 || upstream == Some(401) || body["code"] == "monkeycode_session_expired"
    {
        (
            "token_expired",
            401,
            "MonkeyCode 登录态无效或已过期，请重新登录后粘贴 Session",
        )
    } else if status == 429 || upstream == Some(429) {
        (
            "quota_limited",
            429,
            "MonkeyCode 请求受限，请稍后重试；未确认恢复时间",
        )
    } else {
        (
            "fatal",
            if (400..600).contains(&status) {
                status
            } else {
                502
            },
            "MonkeyCode 请求未完成，未生成替代数据",
        )
    };
    json!({ "kind": kind, "status": http, "message": message,
        "upstream_code": upstream, "reset_at": null })
}
