//! 灵犀的出网封装：统一带 `Cookie: wps_sid=…` 与两个客户端身份头。
//!
//! 代理口径与别家一致：账号级代理（`req.proxy`）优先，其次网关握手给的
//! `env.proxy`（空串 = 直连，`direct` = 显式直连），两者都没有就直连 ——
//! 这条判断**全部交给 [`crate::http::client`]**，不要在本地再写一遍：
//! 插件开了 `system-proxy` 特性，漏掉 `no_proxy()` 会让「直连」悄悄改走
//! 用户系统里配的代理（症状是「设了账号代理反而更慢」）。

use agent2api_provider_protocol as proto;
use serde_json::{json, Value};

use super::constants;

/// 流式请求的总超时。控制面（建会话 / run / 目录 / 余额）用共享客户端的
/// 20 秒；一次对话可能包含长思考与多轮工具，按 10 分钟给。
const STREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// 取一个客户端（按代理缓存、连接复用，口径见 [`crate::http::client`]）。
pub fn build_client(proxy: Option<&proto::WireProxy>, env_proxy: &str) -> reqwest::Client {
    crate::http::client(proxy, env_proxy)
}

/// 非 2xx 的文案（401/403 顺手给「重新粘贴 Cookie」的指路）。
fn status_error(status: u16, detail: &str) -> String {
    let hint = if status == 401 || status == 403 {
        "（WPS 登录态可能已失效，请重新粘贴 Cookie）"
    } else {
        ""
    };
    format!("灵犀上游返回 {status}{hint}：{detail}")
}

fn headers(sid: &str) -> Vec<(&'static str, String)> {
    vec![
        ("cookie", format!("wps_sid={sid}")),
        ("content-type", "application/json".to_string()),
        ("x-client-type", constants::CLIENT_TYPE.to_string()),
        (
            "x-lingxi-appserver-version",
            constants::APPSERVER_VERSION.to_string(),
        ),
        ("referer", format!("{}/", constants::BASE)),
    ]
}

/// `GET` 一个 JSON 接口（信封里 `result:"ok"` / `code:"Success"` 都认）。
pub async fn get_json(client: &reqwest::Client, sid: &str, path: &str) -> Result<Value, String> {
    let mut request = client.get(format!("{}{}", constants::BASE, path));
    for (key, value) in headers(sid) {
        request = request.header(key, value);
    }
    let response = request.send().await.map_err(|error| format!("灵犀请求失败：{error}"))?;
    parse(response).await
}

/// `POST` 一个 JSON 接口。
pub async fn post_json(
    client: &reqwest::Client,
    sid: &str,
    path: &str,
    body: &Value,
    extra: &[(&str, String)],
) -> Result<Value, String> {
    let mut request = client
        .post(format!("{}{}", constants::BASE, path))
        .json(body);
    for (key, value) in headers(sid) {
        request = request.header(key, value);
    }
    for (key, value) in extra {
        request = request.header(*key, value);
    }
    let response = request.send().await.map_err(|error| format!("灵犀请求失败：{error}"))?;
    parse(response).await
}

/// 解信封：非 2xx 或 `result != ok` / `code != Success` 都变成 `Err`。
pub async fn parse(response: reqwest::Response) -> Result<Value, String> {
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|error| format!("读取灵犀响应失败：{error}"))?;
    let payload: Value = serde_json::from_str(&text).unwrap_or(json!({}));
    if !(200..300).contains(&status) {
        return Err(status_error(
            status,
            &error_text(&payload).unwrap_or_else(|| text.chars().take(200).collect()),
        ));
    }
    if let Some(message) = error_text(&payload) {
        return Err(format!("灵犀上游返回错误：{message}"));
    }
    Ok(payload)
}

/// 信封里的错误文案（`result` 非 ok / `code` 非 Success / 有 `message`）。
fn error_text(payload: &Value) -> Option<String> {
    if let Some(result) = payload.get("result").and_then(Value::as_str) {
        if !result.eq_ignore_ascii_case("ok") {
            return Some(text(payload));
        }
    }
    if let Some(code) = payload.get("code").and_then(Value::as_str) {
        if !code.eq_ignore_ascii_case("Success") {
            return Some(text(payload));
        }
    }
    None
}

fn text(payload: &Value) -> String {
    payload
        .get("message")
        .or_else(|| payload.get("msg"))
        .or_else(|| payload.get("hint"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| payload.to_string().chars().take(200).collect())
}

/// 从响应里取 `data` 对象（缺失时回退顶层）。
pub fn data_of(payload: &Value) -> Value {
    payload.get("data").cloned().unwrap_or_else(|| payload.clone())
}

/// 发起一次**流式**请求（不解析 JSON，交给调用方逐块读）。
pub async fn post_stream(
    client: &reqwest::Client,
    sid: &str,
    path: &str,
    body: &Value,
    extra: &[(&str, String)],
) -> Result<reqwest::Response, String> {
    let mut request = client
        .post(format!("{}{}", constants::BASE, path))
        .timeout(STREAM_TIMEOUT)
        .json(body);
    for (key, value) in headers(sid) {
        request = request.header(key, value);
    }
    for (key, value) in extra {
        request = request.header(*key, value);
    }
    let response = request.send().await.map_err(|error| format!("灵犀请求失败：{error}"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let text = response.text().await.unwrap_or_default();
        return Err(status_error(status, &text.chars().take(200).collect::<String>()));
    }
    Ok(response)
}
