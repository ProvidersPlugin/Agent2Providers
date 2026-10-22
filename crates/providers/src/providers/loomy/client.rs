//! Loomy 两种出站请求的薄封装：签名 POST（CAccount）与 token 请求（集成网关）。
//!
//! ── 为什么收在一处 ──────────────────────────────────────────
//! 四处消费者（login / checkin / balance / models）共用同一套超时、错误翻译与
//! 「业务码 → 人话」口径；各写一份会让「哪个码算登录失效」这类判据分叉。
//!
//! ── 业务码的口径（实测 + 客户端源码对照）────────────────────
//! CAccount 与集成网关都用 HTTP 200 + `{code, desc, data}` 表达业务结果：
//!   - `000000`：成功；
//!   - `020002` / `100002`：登录态无效（客户端 `AUTH_ERROR_CODES`）；
//!   - 其余：业务失败，`desc` 是上游文案。

use serde_json::Value;

use super::endpoints;
use super::sign;
use crate::http;

/// 登录态失效业务码（客户端 `AUTH_ERROR_CODES`）
pub const AUTH_ERROR_CODES: [&str; 2] = ["020002", "100002"];

/// 成功码
pub const SUCCESS_CODE: &str = "000000";

/// 读取响应体里的业务码（字符串；缺失时为空串）
pub fn business_code(payload: &Value) -> String {
    payload
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 业务码是不是「登录态无效」
pub fn is_auth_error_code(code: &str) -> bool {
    AUTH_ERROR_CODES.contains(&code)
}

/// 上游业务文案（`desc` → `message` 兜底）
pub fn upstream_message(payload: &Value) -> String {
    payload
        .get("desc")
        .and_then(Value::as_str)
        .or_else(|| payload.get("message").and_then(Value::as_str))
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// 向 CAccount 发一次**带签名**的 POST（登录链路，没有账号上下文，不走代理）。
///
/// 返回上游响应体（`{code, desc, data}`）。HTTP 层失败翻译成错误字符串；
/// 业务码**不在这里判**（调用方各有各的翻译，见 `login.rs`）。
pub async fn signed_post(
    client: &reqwest::Client,
    path: &str,
    body: &Value,
    what: &str,
) -> Result<Value, String> {
    let url = format!("{}{path}", endpoints::xfyun_base_url());
    let headers = sign::signed_headers("POST", path, Some(body))?;
    let (status, payload) = http::post_json(client, &url, &headers, body).await?;
    if !(200..300).contains(&status) {
        return Err(format!(
            "{what}返回 HTTP {status}（上游地址 {}）",
            endpoints::xfyun_base_url()
        ));
    }
    Ok(payload)
}

/// 向集成网关发一次带 `token`（= 登录 session）的请求。
///
/// `path` 形如 `/api/v1/points/first-login`；query 由调用方自己拼在 path 后。
///
/// 成功（HTTP 2xx）返回响应体；HTTP 失败翻译成错误字符串。**登录态失效
/// （业务码）由调用方判定** —— 不同接口的成功码判定不同（模型列表接口根本
/// 不返回 `code`）。
pub async fn token_request(
    client: &reqwest::Client,
    method: &str,
    path: &str,
    session: &str,
    body: Option<&Value>,
    what: &str,
) -> Result<Value, String> {
    if session.trim().is_empty() {
        return Err("Loomy 账号缺少 session".to_string());
    }
    let url = format!("{}{path}", endpoints::integration_base_url());
    let headers = vec![
        ("token".to_string(), session.to_string()),
        ("Authorization".to_string(), format!("Bearer {session}")),
    ];
    let result = match method {
        "POST" => {
            let payload = body.cloned().unwrap_or(Value::Object(serde_json::Map::new()));
            http::post_json(client, &url, &headers, &payload).await
        }
        _ => http::get_json(client, &url, &headers).await,
    };
    let (status, payload) = result?;
    if !(200..300).contains(&status) {
        if status == 401 || status == 403 {
            return Err(format!(
                "{what}：Loomy 登录态已失效，请重新登录（HTTP {status}）"
            ));
        }
        return Err(format!("{what}返回 HTTP {status}"));
    }
    Ok(payload)
}
