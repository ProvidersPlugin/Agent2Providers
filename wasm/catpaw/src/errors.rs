//! 错误不回显 URL、令牌、响应原文；保留 HTTP 与数字业务码，不臆测限额/刷新。

use agent2api_wasm_sdk::{json, Error, Value};

use crate::gates;

pub const INVALID: &str = "catpaw_invalid_request";

pub fn invalid(message: &str) -> Error { Error::http(400, message).with_code(INVALID) }
pub fn protocol(message: &str) -> Error { Error::http(502, message).with_code("catpaw_protocol") }

pub fn upstream(status: u16, business: Option<i64>, operation: &str) -> Error {
    let mut error = if status == 401 {
        Error::http(401, "CatPaw 登录凭据已失效，请重新登录后显式导入；不会自动刷新或更换本机来源")
            .with_code("catpaw_auth")
    } else {
        let status = if (400..500).contains(&status) { status } else { 502 };
        Error::http(status, format!("CatPaw {operation}失败，请检查登录凭据与网络（HTTP {status}）"))
            .with_code("catpaw_upstream")
    };
    error.upstream_code = business;
    error
}

pub fn host_stop(error: &Error) -> bool {
    matches!(error.code.as_deref(), Some("aborted" | "forbidden" | "plugin_resource_limit"
        | "plugin_protocol_error" | "plugin_unavailable" | "plugin_trap" | "native_helper_not_authorized"))
}

pub fn host(mut error: Error) -> Error {
    let (status, message) = match error.code.as_deref() {
        Some("aborted") => (499, "CatPaw 操作已由主程序取消"),
        Some("forbidden" | "native_helper_not_authorized") => (403, "CatPaw 所需主程序能力未获授权"),
        Some("plugin_resource_limit") => (503, "CatPaw 操作超过主程序资源限制"),
        Some("plugin_protocol_error" | "plugin_trap") => (502, "CatPaw 主程序能力返回异常"),
        Some("timeout") => (504, "CatPaw 操作超时，请检查网络与账号代理"),
        Some("desktop_credentials_unavailable") => (503, "CatPaw 本机登录态不可读、未登录或格式不支持；请先在网关电脑上的官方客户端登录，或改用网页登录与手工快照"),
        _ => (503, "CatPaw 所需主程序能力暂不可用"),
    };
    error.message = message.into();
    error.status = Some(status);
    error
}

pub fn transport(error: Error) -> Error {
    if host_stop(&error) || error.code.as_deref() == Some("timeout") { return host(error); }
    Error::http(502, "CatPaw 请求或响应读取失败，请检查网络与账号代理；未回退其他账号或直连出口")
        .with_code("catpaw_transport")
}

pub fn contains_secret(text: &str, secrets: &[&str]) -> bool {
    secrets.iter().filter(|secret| !secret.is_empty()).any(|secret| text.contains(*secret))
}

pub fn public_text(text: &str, secrets: &[&str], limit: usize) -> String {
    if contains_secret(text, secrets) { return "[已隐藏]".into(); }
    crate::util::safe_text(text, limit)
}

pub fn is_error_response(status: u64, body: &Value) -> bool {
    status >= 400 || body.get("error").is_some_and(|value| !value.is_null())
        || body["success"] == false || body["type"] == "error"
        || body.get("code").is_some_and(|value| !value.is_null()
            && !matches!(value.as_i64(), Some(0 | 200)))
}

fn failure_code(body: &Value) -> &str {
    body["code"].as_str().or_else(|| body["providerCode"].as_str())
        .or_else(|| body["error"]["code"].as_str())
        .or_else(|| body["details"]["code"].as_str()).unwrap_or("")
}

pub fn turn(mut error: Error) -> Error {
    let (status, message) = match error.code.as_deref() {
        Some("catpaw_account_credentials") => (401, "CatPaw 保存的账号凭据或身份无效，请重新导入"),
        Some("catpaw_account_user_agent") => (400, "CatPaw 账号的请求标识格式无效，请检查账号设置"),
        Some("catpaw_account_binding" | "catpaw_turn_invalid_input") => (400, "CatPaw 本次账号或轮次材料不符合要求，未重发请求"),
        Some("catpaw_call_material_invalid" | "catpaw_call_projection_mismatch" | "catpaw_call_projection_unexpressed") => (400, "CatPaw 本次请求包含尚未接入或互相冲突的内容与参数，未忽略要求或重发请求"),
        Some("catpaw_call_material_limit") => (413, "CatPaw 本次请求材料超过大小或结构限制，未截断发送"),
        Some("catpaw_account_changed" | "catpaw_turn_already_started" | "catpaw_turn_session_busy" | "catpaw_turn_read_conflict") => (409, "CatPaw 原账号或会话状态已变化，未替换会话或重发请求"),
        Some("catpaw_account_limit" | "catpaw_turn_resource_limit") => (413, "CatPaw 本次账号或轮次材料超过大小限制"),
        Some("catpaw_account_timeout" | "timeout") => (504, "CatPaw 操作超时，请检查网络与账号代理"),
        Some("catpaw_account_call_ended" | "catpaw_turn_call_ended" | "aborted") => (499, "CatPaw 本次调用已结束或取消，未重发请求"),
        Some("request_call_revoked") => (503, "CatPaw 原调用已结束或执行体已切换，未重发请求"),
        Some("catpaw_model_source_unconfirmed") => (503, "CatPaw 本次账号目录未确认模型或档位，请刷新模型目录后重试"),
        Some("forbidden" | "native_helper_not_authorized") => (403, "CatPaw 所需主程序能力未获授权"),
        Some("desktop_credentials_unavailable") => (503, "CatPaw 本机登录态不可读或格式不支持，请先在网关电脑上的官方客户端登录"),
        Some("catpaw_turn_upstream_rejected") => (502, "CatPaw 上游拒绝了本次轮次请求，未自动重发"),
        Some("catpaw_turn_submission_unconfirmed") => (502, "CatPaw 本次轮次提交结果未确认，未将未知结果当成未发送并重试"),
        Some("catpaw_turn_stream_unconfirmed" | "catpaw_turn_protocol_unconfirmed" | "catpaw_turn_read_delivery_unconfirmed") => (502, "CatPaw 上游响应未完整结束，未将部分内容当成成功"),
        Some("catpaw_turn_tool_evidence_invalid") => (502, "CatPaw 上游消息的字段或工具材料不完整，未忽略异常继续发送"),
        Some("catpaw_turn_tool_result_candidate_invalid") => (400, "CatPaw 工具结果与本次全部工具调用不一致，未补造结果或重发"),
        Some("catpaw_output_unconfirmed") => (502, "CatPaw 最终输出未完整核对，未宣布交付成功或开放续接"),
        Some("catpaw_turn_settlement_evidence_invalid") => (502, "CatPaw 本次完整响应与结算材料不一致，未把部分内容当成成功"),
        Some("catpaw_turn_settlement_unconfirmed") => (502, "CatPaw 本次完成受理或状态提交未确认，未重复完成或重发请求"),
        Some("catpaw_turn_output_unconfirmed") => (499, "CatPaw 下游输出已结束或未确认，未重发上游轮次"),
        Some("catpaw_turn_cleanup_unconfirmed") => (502, "CatPaw 本次轮次收尾未确认，未猜测已停止或重发请求"),
        Some("catpaw_account_busy" | "catpaw_account_unavailable" | "catpaw_turn_source_unavailable") => (503, "CatPaw 原账号来源或运行通道暂不可用，未替换来源"),
        _ => {
            error.code = Some("catpaw_turn_host_unavailable".into());
            (503, "CatPaw 受控会话能力不可用或操作未确认，未回退普通网络请求")
        }
    };
    error.message = message.into();
    error.status = Some(status);
    error
}

pub fn classify(status: u16, body: &Value) -> Value {
    let code = failure_code(body);
    let turn_message = turn(Error::coded("", code));
    let message = match code {
        "catpaw_conversation_host_unavailable" => gates::CHAT,
        "catpaw_login_host_unavailable" => gates::WEB_LOGIN,
        "catpaw_desktop_source_unavailable" => gates::DESKTOP,
        "catpaw_refresh_unavailable" => gates::REFRESH,
        "desktop_credentials_unavailable" => "CatPaw 本机登录态不可读或格式不支持，请先在网关电脑上的官方客户端登录",
        "catpaw_auth" => "CatPaw 登录凭据已失效，请重新登录；本机来源请在网关电脑上的官方客户端重新登录",
        "catpaw_invalid_request" => "CatPaw 请求格式不符合要求，换账号无法解决",
        code if code.starts_with("catpaw_turn_") || code.starts_with("catpaw_account_") || code.starts_with("catpaw_call_") => &turn_message.message,
        _ => "CatPaw 操作失败，未根据状态码推断额度耗尽或自动续期",
    };
    let upstream = body["upstreamCode"].as_i64().or_else(|| body["upstream_code"].as_i64())
        .or_else(|| body["code"].as_i64());
    json!({ "kind": "fatal", "status": if (400..600).contains(&status) { status } else { 502 },
        "message": message, "upstream_code": upstream })
}

pub fn policy(body: &Value) -> Value {
    if matches!(failure_code(body), "catpaw_invalid_request" | "catpaw_conversation_host_unavailable"
        | "catpaw_login_host_unavailable" | "catpaw_desktop_source_unavailable" | "catpaw_refresh_unavailable"
        | "catpaw_activity_unverified" | "catpaw_call_material_invalid" | "catpaw_call_projection_mismatch"
        | "catpaw_call_projection_unexpressed" | "catpaw_call_material_limit") {
        json!({ "stop": true, "reason": "本次属于本地输入或未接入的托管能力，换账号无法解决" })
    } else { json!({ "stop": false }) }
}
