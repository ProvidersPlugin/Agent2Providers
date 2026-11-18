//! 只保留安全错误码，不回显上游正文/URL/令牌；HTTP 成功不是业务成功。
//! 403 仅说明这次读取被拒，不猜封号；所有基础读取不自动续期、跨站或换设备。

use crate::endpoints::ReadEndpoint;
use agent2api_wasm_sdk::{json, Error, Value};

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
                | "monkeycode_intl_account_changed"
                | "monkeycode_intl_clock_unavailable"
                | "monkeycode_intl_deadline_exceeded"
        )
    )
}

pub fn transport(error: Error, endpoint: ReadEndpoint) -> Error {
    // 任意未知宿主码不能携带到公开响应，防止它含输入内容；已知停止码原样保留。
    let code = if host_stop(&error) {
        error.code.clone()
    } else {
        Some("monkeycode_intl_transport_error".into())
    };
    let mut output = Error::http(
        error
            .status
            .filter(|status| (400..600).contains(status))
            .unwrap_or(502),
        format!(
            "MonkeyCode 国际{}未完成（网络或主程序限制），没有使用猜测数据",
            endpoint.label()
        ),
    );
    output.code = code;
    output
}

pub fn protocol(endpoint: ReadEndpoint, detail: &str) -> Error {
    Error::http(
        502,
        format!(
            "MonkeyCode 国际{}响应格式未确认：{detail}；未用默认值代替",
            endpoint.label()
        ),
    )
    .with_code("monkeycode_intl_protocol_error")
}

pub fn upstream(status: u16, code: Option<i64>, endpoint: ReadEndpoint) -> Error {
    let interpreted = if (200..300).contains(&status) {
        code
    } else {
        None
    };
    let (http, category, message) = if status == 401 || interpreted == Some(401) {
        (
            401,
            "monkeycode_intl_session_invalid",
            "登录态无效或已过期，请在国际站重新登录后手工导入",
        )
    } else if status == 403 || matches!(interpreted, Some(403 | 10001 | 10202 | 10605 | 10812)) {
        (
            403,
            "monkeycode_intl_forbidden",
            "本次访问被拒，原因未确认；没有推断封号或绕过限制",
        )
    } else if matches!(status, 404 | 405 | 501) {
        (
            status,
            "monkeycode_intl_endpoint_unavailable",
            "当前端点未完成请求；不能据此断言上游永远没有该能力",
        )
    } else if interpreted == Some(10811) {
        (
            409,
            "monkeycode_intl_task_busy",
            "原账号任务名额已占用，未枚举或停止其他任务",
        )
    } else if status == 429 || interpreted == Some(429) {
        (
            429,
            "monkeycode_intl_rate_limited",
            "本次读取受限，未猜测额度、重置时间或自动重试",
        )
    } else if (300..400).contains(&status) {
        (
            502,
            "monkeycode_intl_redirect_rejected",
            "收到重定向，不跟随到登录页或其它来源",
        )
    } else {
        (
            if (400..600).contains(&status) {
                status
            } else {
                502
            },
            "monkeycode_intl_upstream_error",
            "读取失败，未生成替代数据",
        )
    };
    let mut error = Error::http(
        http,
        format!(
            "MonkeyCode 国际{}：{message}（HTTP {status}，业务码 {}）",
            endpoint.label(),
            code.map(|code| code.to_string())
                .unwrap_or_else(|| "未知".into())
        ),
    )
    .with_code(category);
    error.upstream_code = code;
    error
}

/// SDK/宿主错误也不能经由账户/状态读取旁路带出凭据；已知安全包内错误保持原样。
pub fn sanitize(error: Error) -> Error {
    let code = error.code.as_deref().unwrap_or("");
    if code.starts_with("monkeycode_intl_")
        || code.starts_with("monkeycode_task_")
        || code.starts_with("monkeycode_conversation_")
        || code.starts_with("monkeycode_remote_activity_")
        || code == "unsupported_parameter"
        || code == "monkeycode_cap_unavailable"
    {
        return error;
    }
    let mut output = Error::http(
        error
            .status
            .filter(|status| (400..600).contains(status))
            .unwrap_or(502),
        "MonkeyCode 国际操作未完成；输入、主程序权限或读取状态不满足要求，未回显原始错误内容",
    );
    output.upstream_code = error.upstream_code;
    output.reset_at = error.reset_at;
    output.code = if host_stop(&error) || code == "unsupported" {
        error.code
    } else {
        Some("monkeycode_intl_operation_failed".into())
    };
    output.credential_scope = error.credential_scope;
    output
}

pub fn report(error: &Error) -> Value {
    json!({ "status": error.status, "code": error.code, "upstreamCode": error.upstream_code, "message": error.message })
}

pub fn replay_forbidden(body: &Value) -> bool {
    ["code", "providerCode", "provider_code"]
        .into_iter()
        .any(|key| {
            body[key].as_str().is_some_and(|code| {
                code.starts_with("monkeycode_task_")
                    || code.starts_with("monkeycode_remote_activity_")
                    || code == "monkeycode_conversation_bad_input"
                    || code == "unsupported_parameter"
            })
        })
}

pub fn classify(status: u16, body: &Value) -> Value {
    let upstream = body["upstreamCode"]
        .as_i64()
        .or_else(|| body["upstream_code"].as_i64())
        .or_else(|| body["code"].as_i64());
    let expired = status == 401
        || (200..300).contains(&status) && upstream == Some(401)
        || status != 403
            && ["code", "providerCode", "provider_code"]
                .into_iter()
                .any(|key| body[key] == "monkeycode_intl_session_invalid");
    let busy = upstream == Some(10811)
        || ["code", "providerCode", "provider_code"]
            .into_iter()
            .any(|key| body[key] == "monkeycode_task_busy");
    let quota = !busy && (status == 429 || upstream == Some(429));
    json!({ "kind": if expired { "token_expired" } else if quota { "quota_limited" } else { "fatal" },
        "status": if busy { 409 } else if expired { 401 } else if quota { 429 } else if (400..600).contains(&status) { status } else { 502 },
        "message": if expired { "MonkeyCode 国际登录态无效，请重新登录" }
            else if busy { "MonkeyCode 国际任务名额已占用，未枚举或停止其他任务" }
            else if quota { "MonkeyCode 国际请求受限，请稍后重试" }
            else { "MonkeyCode 国际请求未完成，未生成替代数据或回落其他地区" },
        "upstream_code": upstream, "reset_at": null })
}
