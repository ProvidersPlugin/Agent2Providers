//! 固定错误文案，不把 Cookie、设备信息、挑战载荷、URL 或上游 message 带到公开出口。

use agent2api_wasm_sdk::{json, Error, Value};

use crate::{endpoints::ReadEndpoint, util};

pub fn invalid(message: &str) -> Error {
    Error::http(400, message).with_code("doubaowork_invalid_input")
}
pub fn device_context() -> Error {
    Error::http(
        400,
        "豆包工作登录快照缺少设备编号，请重新导入本机登录态，或在 Cookie JSON 中补充原登录客户端的 deviceId；未发送不完整请求，也不会读取或借用其他设备信息",
    )
    .with_code("doubaowork_device_context_missing")
}

pub fn protocol(message: &str) -> Error {
    Error::http(502, message).with_code("doubaowork_invalid_response")
}
pub fn changed() -> Error {
    Error::http(
        409,
        "豆包工作账号身份或凭据在操作期间发生变化，请重新查询；未采用其他登录态",
    )
    .with_code("doubaowork_account_changed")
}
pub fn identity() -> Error {
    Error::http(
        401,
        "豆包工作账号身份与保存的登录态不一致，请重新显式导入；未更换账号",
    )
    .with_code("doubaowork_identity_mismatch")
}

pub fn host_stop(error: &Error) -> bool {
    matches!(
        error.code.as_deref(),
        Some(
            "aborted"
                | "forbidden"
                | "timeout"
                | "cancelled"
                | "canceled"
                | "deadline_exceeded"
                | "permission_denied"
                | "plugin_resource_limit"
                | "plugin_protocol_error"
                | "plugin_unavailable"
                | "plugin_trap"
                | "doubaowork_account_changed"
                | "doubaowork_identity_mismatch"
        )
    )
}

pub fn host(error: Error) -> Error {
    let (status, code, message) = match error.code.as_deref() {
        Some("aborted" | "cancelled" | "canceled") => (499, "aborted", "豆包工作操作已取消"),
        Some("forbidden" | "permission_denied") => {
            (403, "forbidden", "豆包工作所需主程序能力未获授权")
        }
        Some("timeout" | "deadline_exceeded") => {
            (504, "timeout", "豆包工作操作超时，未使用猜测结果")
        }
        Some("plugin_resource_limit") => (
            503,
            "plugin_resource_limit",
            "豆包工作操作超过主程序资源限制",
        ),
        Some("plugin_protocol_error" | "plugin_trap") => {
            (502, "plugin_protocol_error", "豆包工作主程序能力返回异常")
        }
        _ => (503, "plugin_unavailable", "豆包工作所需主程序能力暂不可用"),
    };
    Error::http(status, message).with_code(code)
}

pub fn transport(error: Error) -> Error {
    if host_stop(&error) {
        return host(error);
    }
    Error::http(
        502,
        "豆包工作请求或响应读取失败，请检查网络与账号代理；未直连回退或更换账号",
    )
    .with_code("doubaowork_transport")
}

pub fn business(value: &Value) -> Option<i64> {
    let detail = value
        .get("error_detail")
        .filter(|value| value.is_object())
        .or_else(|| value.get("error").filter(|value| value.is_object()))
        .unwrap_or(value);
    util::integer(&detail["code"])
        .or_else(|| util::integer(&detail["error_code"]))
        .or_else(|| util::integer(&value["code"]))
        .or_else(|| util::integer(&value["error_code"]))
}

pub fn verification(value: &Value) -> bool {
    let decision = value
        .get("extra")
        .and_then(|extra| extra.get("decision"))
        .or_else(|| value.get("decision"));
    match decision {
        Some(Value::Object(fields)) => fields.get("type").and_then(Value::as_str) == Some("verify"),
        Some(Value::String(raw)) if raw.len() <= 64 * 1024 => serde_json::from_str::<Value>(raw)
            .ok()
            .is_some_and(|value| value["type"] == "verify"),
        _ => false,
    }
}

pub fn upstream(status: u16, body: &Value, endpoint: ReadEndpoint) -> Error {
    let code = business(body);
    let detail = body
        .get("error_detail")
        .filter(|value| value.is_object())
        .or_else(|| body.get("error").filter(|value| value.is_object()))
        .unwrap_or(body);
    let message = util::text(detail, &["message", "msg", "error_msg", "error_message"])
        .chars()
        .take(2048)
        .collect::<String>()
        .to_ascii_lowercase();
    if let Some(error) = reported(body) {
        return error;
    }
    let mut error = if verification(body) {
        Error::http(
            429,
            "豆包工作要求在官方页面完成人机验证，这不是已确认的额度耗尽；未重放请求",
        )
        .with_code("doubaowork_verification_required")
    } else if status == 401
        || code == Some(710012001)
        || message.contains("not logged in")
        || message.contains("login required")
    {
        Error::http(
            401,
            "豆包工作登录态已失效，请重新登录并显式导入 Cookie；本包不会自动续期",
        )
        .with_code("doubaowork_auth_expired")
    } else if message.contains("insufficient credit")
        || message.contains("insufficient quota")
        || message.contains("quota exceeded")
        || message.contains("积分不足")
    {
        Error::http(429, "豆包工作当前账号额度不足，请查询额度或稍后重试")
            .with_code("doubaowork_quota_limited")
    } else if message.contains("content policy")
        || message.contains("content blocked")
        || message.contains("content filter")
    {
        Error::http(403, "豆包工作内容策略拒绝了本次请求").with_code("doubaowork_content_blocked")
    } else if status == 429 || code == Some(710022004) {
        Error::http(
            429,
            "豆包工作返回请求受限，但该响应不能区分频率限制、设备上下文异常或上游验证；请先核对原登录快照与官方客户端状态，未据此判定额度耗尽或重放请求",
        )
        .with_code("doubaowork_request_limited")
    } else {
        let reported = if (400..600).contains(&status) {
            status
        } else {
            502
        };
        Error::http(
            reported,
            format!(
                "豆包工作{}失败（HTTP {status}），未公开敏感响应或生成替代结果",
                endpoint.label()
            ),
        )
        .with_code("doubaowork_upstream_error")
    };
    error.upstream_code = code;
    error
}

pub fn reported(body: &Value) -> Option<Error> {
    let detail = body
        .get("error")
        .filter(|value| value.is_object())
        .unwrap_or(body);
    let encoded = detail["code"]
        .as_str()
        .or_else(|| body["code"].as_str())
        .or_else(|| body["providerCode"].as_str())?;
    let metadata = encoded
        .strip_prefix("provider_http:")
        .filter(|raw| raw.len() <= 8192)
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    let code = metadata
        .as_ref()
        .and_then(|metadata| metadata["code"].as_str())
        .unwrap_or(encoded);
    let mut error = match code {
        "doubaowork_device_context_missing" => device_context(),
        "doubaowork_subscription_required" => Error::http(403,
            "豆包工作官方确认当前账号不能使用此订阅专属模型；这不是登录过期，也不是已确认的请求频率限制")
            .with_code(code),
        "doubaowork_model_restricted" => Error::http(403,
            "豆包工作官方限制了当前模型的使用；请在官方客户端核对订阅与模型权限，未将限制提示当作正常回答")
            .with_code(code),
        "doubaowork_invalid_input" | "unsupported_parameter" => Error::http(400,
            "豆包工作请求参数无效或不适用于当前模型，请核对所选模型和官方支持的参数").with_code(code),
        "doubaowork_invalid_response" => protocol("豆包工作返回的目录或回答格式与已接通协议不一致，未生成替代结果"),
        "doubaowork_transport" => Error::http(502,
            "豆包工作请求或响应读取失败，请检查网络与账号代理；未直连回退或更换账号").with_code(code),
        "doubaowork_auth_expired" => Error::http(401,
            "豆包工作登录态已失效，请重新登录并显式导入 Cookie；本包不会自动续期").with_code(code),
        "doubaowork_verification_required" => Error::http(429,
            "豆包工作要求在官方页面完成人机验证，这不是已确认的额度耗尽；未重放请求").with_code(code),
        "doubaowork_request_limited" => Error::http(429,
            "豆包工作返回请求受限，但该响应不能区分频率限制、设备上下文异常或上游验证；请先核对原登录快照与官方客户端状态，未据此判定额度耗尽或重放请求").with_code(code),
        "doubaowork_content_blocked" => Error::http(403,
            "豆包工作内容策略拒绝了本次请求").with_code(code),
        "doubaowork_quota_limited" => Error::http(429,
            "豆包工作当前账号额度不足，请查询额度或稍后重试").with_code(code),
        _ => return None,
    };
    error.upstream_code = detail
        .get("upstream_code")
        .or_else(|| body.get("upstream_code"))
        .or_else(|| body.get("upstreamCode"))
        .and_then(util::integer)
        .or_else(|| {
            metadata
                .as_ref()
                .and_then(|metadata| util::integer(&metadata["upstreamCode"]))
        });
    Some(error)
}

pub fn report(error: &Error) -> Value {
    json!({ "status": error.status, "code": error.code, "upstreamCode": error.upstream_code,
        "message": error.message })
}

pub fn classify(status: u16, body: &Value) -> Value {
    let error = upstream(status, body, ReadEndpoint::Chat);
    let kind = match error.code.as_deref() {
        Some("doubaowork_auth_expired") => "token_expired",
        Some("doubaowork_quota_limited") => "quota_limited",
        Some("doubaowork_content_blocked") => "content_blocked",
        _ if error.status == Some(401) => "token_expired",
        _ => "fatal",
    };
    json!({ "kind": kind,
        "status": error.status.unwrap_or(502), "reset_at": null, "message": error.message, "upstream_code": error.upstream_code })
}
