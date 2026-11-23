//! 国内账号与模型接口共用受控网络出口，所有地址由本包常量拼出。
//! 失败只输出动作名/HTTP/业务码；不回显可能含账号令牌的上游文案或 URL。
use super::{endpoints, headers, util};
use agent2api_wasm_sdk::{http, Error, Result, Value};

pub struct Reply {
    pub status: u16,
    pub body: Value,
}

pub fn request(
    method: &str,
    path: &str,
    token: &str,
    body: Option<&Value>,
    operation: &str,
) -> Result<Reply> {
    request_with_headers(method, path, headers::signed(token)?, body, operation)
}
fn request_with_headers(
    method: &str,
    path: &str,
    headers: Value,
    body: Option<&Value>,
    operation: &str,
) -> Result<Reply> {
    if !path.starts_with('/') || path.starts_with("//") || path.contains(['\r', '\n', '\0', '#']) {
        return Err(Error::coded("AutoClaw 请求路径无效", "forbidden"));
    }
    let result = (|| -> Result<Reply> {
        let timeout = if path == endpoints::MODELS || path == "/userapi/v1/user-profile" {
            15_000
        } else {
            20_000
        };
        let response = http::open_timeout(
            method,
            &format!("{}{path}", endpoints::ORIGIN),
            headers,
            body,
            timeout,
        )?;
        let status = response.status;
        let bytes = response.bytes(4 * 1024 * 1024)?;
        let body = super::errors::parse_body(&bytes);
        Ok(Reply { status, body })
    })();
    result.map_err(|mut error| {
        if error.code.as_deref() != Some("forbidden") {
            let raw = error.message.trim().to_string();
            error.message = if raw.is_empty() {
                format!("AutoClaw {operation}连接失败或响应中断")
            } else {
                format!("AutoClaw {operation}连接失败：{raw}")
            };
        }
        error
    })
}

pub fn http_ok(reply: Reply, operation: &str) -> Result<Value> {
    if !(200..300).contains(&reply.status) {
        return Err(super::errors::failure(reply.status, &reply.body));
    }
    if !reply.body.is_object() {
        return Err(Error::http(
            502,
            super::errors::append_detail(
                format!("AutoClaw {operation}未返回有效 JSON（HTTP {}）", reply.status),
                &reply.body,
            ),
        ));
    }
    Ok(reply.body)
}
pub fn get(path: &str, token: &str, operation: &str) -> Result<Value> {
    http_ok(request("GET", path, token, None, operation)?, operation)
}
pub fn post(path: &str, token: &str, body: &Value, operation: &str) -> Result<Value> {
    http_ok(
        request("POST", path, token, Some(body), operation)?,
        operation,
    )
}
pub fn activate(token: &str, body: &Value) -> Result<Value> {
    http_ok(
        request_with_headers(
            "POST",
            "/userapi/v1/user-info",
            headers::active(token)?,
            Some(body),
            "登录活跃触发",
        )?,
        "登录活跃触发",
    )
}
pub fn auth_code(code: i64) -> bool {
    matches!(code, 401 | 410_000 | 400_000)
}
pub fn business(payload: &Value, operation: &str) -> Result<()> {
    // task-complete / task-list 等旧响应可以只有 data；不存在的 code 与无效 code 不能混为一谈。
    if payload.get("code").is_none() && payload.get("data").is_some() {
        return Ok(());
    }
    strict_business(payload, operation)
}
pub fn strict_business(payload: &Value, operation: &str) -> Result<()> {
    let code = util::integer(&payload["code"]);
    if code == Some(0) {
        return Ok(());
    }
    let status = if code == Some(410_004) {
        403
    } else if code.is_some_and(auth_code) {
        401
    } else {
        502
    };
    let code_text = code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "未返回".into());
    let base = if code == Some(410_004) {
        format!("AutoClaw 账号被上游停用或封禁，请联系官方处理（业务码 {code_text}）")
    } else if status == 401 {
        format!("AutoClaw 登录态已失效，无法{operation}（业务码 {code_text}）")
    } else {
        format!("AutoClaw {operation}未完成（业务码 {code_text}）")
    };
    let mut error = Error::http(status, super::errors::append_detail(base, payload));
    error.upstream_code = code;
    Err(error)
}
