//! OrcaTerm 出站请求：JSON 动作（CGI）与 SSE 对话流。
//!
//! ── 为什么收在一处 ──────────────────────────────────────────
//! CGI 网关（`cgi/api`）与 assistant 网关（`lightai`）共用同一套鉴权头
//! （`Authorization: Bearer <JWT>`）与代理/超时/错误翻译口径；各写一份会让
//! 「哪个状态码算登录失效」这类判据分叉。
//!
//! ── 错误翻译口径 ────────────────────────────────────────────
//!   - 连接 / 超时 → 502 / 504；
//!   - HTTP 401 / 403 → 401（JWT 失效，调用方走「强制刷新后重试一次」）；
//!   - 其余非 2xx → 原状态码 + 响应体前 240 字符。
//! CGI 的业务错误**不在这里判**：它 HTTP 恒 200，业务码在
//! `data.Response.Error.Code`，由调用方各自翻译。

use serde_json::Value;

use super::USER_AGENT;
use crate::http;

/// JSON 请求的默认超时（建会话 / 续期 / 余额这类管理接口）。
const REQUEST_TIMEOUT_MS: u64 = 20_000;

/// 一次 JSON POST，返回已解析的 JSON。
///
/// ── 为什么错误是 `String` 而不是网关的 `GatewayError` ─────────
/// 插件侧没有 `GatewayError`（那是网关的客户端形态）。需要区分状态码的
/// 地方由调用方看 [`HttpFailure::status`]；不需要的地方直接用文案。
pub async fn post_json(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> Result<Value, HttpFailure> {
    let mut request = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("User-Agent", USER_AGENT)
        .timeout(std::time::Duration::from_millis(REQUEST_TIMEOUT_MS))
        .body(body.to_string());
    for (name, value) in headers {
        request = request.header(name.as_str(), value);
    }
    let response = request.send().await.map_err(|error| HttpFailure {
        status: if error.is_timeout() { 504 } else { 502 },
        message: http::describe(&error),
    })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(translate(status, url, &text));
    }
    serde_json::from_str(&text).map_err(|error| HttpFailure {
        status: 502,
        message: format!("OrcaTerm 返回非 JSON 响应（{url}）：{error}"),
    })
}

/// 一次失败（带状态码，供调用方判 401）。
#[derive(Clone, Debug)]
pub struct HttpFailure {
    /// HTTP 语义的状态码（传输错误给 502 / 504）。
    pub status: u16,
    /// 客户端可见的文案。
    pub message: String,
}

impl HttpFailure {
    /// 是不是「登录态失效」（调用方据此强制续期后重试一次）。
    pub fn is_auth(&self) -> bool {
        self.status == 401 || self.status == 403
    }
}

/// 非 2xx 的统一翻译。
fn translate(status: u16, url: &str, body: &str) -> HttpFailure {
    if status == 401 || status == 403 {
        return HttpFailure {
            status: 401,
            message: format!(
                "OrcaTerm 登录态已失效，请重新导入或重新粘贴该账号的 accessToken（HTTP {status}，{url}）"
            ),
        };
    }
    let head: String = body.chars().take(240).collect();
    HttpFailure {
        status,
        message: format!("OrcaTerm 返回 HTTP {status}（{url}）：{head}"),
    }
}

/// 发起一次 SSE 流式 POST（对话流；**不设总超时**，长推理不被截断）。
///
/// 返回原始 `reqwest::Response`（调用方遍历 `bytes_stream()`）。空闲兜底由
/// 调用方做 —— 它才知道「多久没数据算卡住」。
pub async fn sse_post(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
    what: &str,
) -> Result<reqwest::Response, HttpFailure> {
    let result = http::post_stream(client, url, headers, body, "text/event-stream")
        .await
        .map_err(|message| HttpFailure { status: 502, message: format!("{what}失败：{message}") })?;
    if !result.is_success() {
        if result.status == 401 || result.status == 403 {
            return Err(HttpFailure {
                status: 401,
                message: format!(
                    "OrcaTerm 登录态已失效，请重新导入或重新粘贴该账号的 accessToken（HTTP {}）",
                    result.status
                ),
            });
        }
        return Err(HttpFailure {
            status: result.status,
            message: format!("OrcaTerm 返回 HTTP {}（{what}）：{}", result.status, result.error_head()),
        });
    }
    result.response.ok_or_else(|| HttpFailure {
        status: 502,
        message: format!("{what}没有得到响应体"),
    })
}
