//! ModelScope 两种出站请求的薄封装：网页站（cookie 鉴权）与推理域（Bearer）。
//!
//! ── 为什么收在一处 ──────────────────────────────────────────
//! 四处消费者（`checkin` / `balance` / `models` / 账号添加时的探活）共用同一套
//! 超时、错误翻译与「响应信封 → 人话」口径；各写一份会让「哪个码算令牌失效」
//! 这类判据分叉。
//!
//! ── 上游的**两种信封**（实测，都出现在同一家的不同接口上）──
//! 这是本文件最需要注意的一处：魔搭自己的接口没统一大小写。
//!
//! ```text
//!   /api/v1/users/login/info        {"Code":200,"Success":true,"Data":{…},"Message":"success"}
//!   /api/v1/inference/rate-limit    {"Code":200,"Success":true,"Data":{…}}
//!   /openapi/v1/magicubes/balance   {"success":true,"data":{…}}            ← 全小写
//! ```
//!
//! 因此取值一律**两种写法都认**（[`succeeded`] / [`data_of`] / [`message_of`]），
//! 不要只按一种写死 —— 那会让某一条链路把成功读成失败（或反之）。
//!
//! ── 令牌失效的判据 ──────────────────────────────────────────
//! 网页站对无效令牌给的是 **HTTP 401 + `{"Code":10019901001,"Message":"access
//! forbidden"}`**。也有接口用 HTTP 200 + `Success:false` 表达失败 —— 两条路
//! 都要认：HTTP 401/403 → 调用方按令牌失效处置，业务位为假 → 502 并把上游
//! 文案带出来。

use serde_json::Value;

use super::region::Region;
use crate::http;

/// 响应信封的**成功位**（大写 `Success` 与小写 `success` 都认）。
///
/// 两种都没有时看 `Code`（缺失则视为成功 —— HTTP 已经 2xx 了，没有反证
/// 就照实报成功）。
pub fn succeeded(payload: &Value) -> bool {
    if let Some(flag) = payload.get("Success").and_then(Value::as_bool) {
        return flag;
    }
    if let Some(flag) = payload.get("success").and_then(Value::as_bool) {
        return flag;
    }
    match payload.get("Code").and_then(Value::as_i64) {
        Some(code) => code == 200,
        None => true,
    }
}

/// 响应信封里的**业务数据**（`Data` / `data` 两层都认）。
pub fn data_of(payload: &Value) -> Value {
    payload
        .get("Data")
        .or_else(|| payload.get("data"))
        .cloned()
        .unwrap_or(Value::Null)
}

/// 响应信封里的**上游文案**（`Message` / `message` / `msg` 三种都认）。
pub fn message_of(payload: &Value) -> String {
    payload
        .get("Message")
        .or_else(|| payload.get("message"))
        .or_else(|| payload.get("msg"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// 响应信封里的业务码（`Code` / `code`；都没有时给 0）。
pub fn code_of(payload: &Value) -> i64 {
    payload
        .get("Code")
        .or_else(|| payload.get("code"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

/// 拼一个网页站 URL（`region` 决定站点）
pub fn site_url(region: Region, path: &str) -> String {
    format!("{}{path}", region.site_base_url())
}

/// 向**网页站**发一次带会话 cookie 的 GET。
///
/// 鉴权是 `Cookie: m_session_id=<令牌>`（实测口径）。
///
/// 返回 `(status, payload)`；**业务位不在本函数里判** —— 调用方各自知道
/// 「这次调用的成功长什么样」（签到只看 HTTP，余额看业务位）。
pub async fn site_get(
    region: Region,
    path: &str,
    token: &str,
    what: &str,
    gateway_proxy: &str,
) -> Result<(u16, Value), String> {
    if token.trim().is_empty() {
        return Err("ModelScope 账号缺少令牌，无法查询".to_string());
    }
    let url = site_url(region, path);
    let headers = vec![("Cookie".to_string(), format!("m_session_id={token}"))];
    let client = http::client(None, gateway_proxy);
    let (status, payload) = http::get_json(&client, &url, &headers)
        .await
        .map_err(|error| {
            if error.contains("超时") {
                format!("{what}超时，请稍后重试")
            } else {
                format!("{what}失败：{error}")
            }
        })?;
    Ok((status, payload))
}

/// 向**推理域**发一次带 Bearer 的 GET（目前只有模型目录用）。
///
/// 与 [`site_get`] 是**两个域**：网页站挂了会连签到一起失败，而推理域挂了只
/// 影响模型目录 —— 分开成两个函数，才不会让排障时误判成同一处故障。
pub async fn inference_get(
    region: Region,
    path: &str,
    token: &str,
    what: &str,
    gateway_proxy: &str,
) -> Result<(u16, Value), String> {
    if token.trim().is_empty() {
        return Err("ModelScope 账号缺少令牌，无法查询".to_string());
    }
    let url = format!("{}{path}", region.inference_base_url());
    let headers = vec![("Authorization".to_string(), format!("Bearer {token}"))];
    let client = http::client(None, gateway_proxy);
    http::get_json(&client, &url, &headers)
        .await
        .map_err(|error| format!("{what}失败：{error}"))
}

/// 令牌失效的统一文案（网页站与推理域共用一个判据：401 / 403）。
///
/// 打错站点时上游给的就是 401 —— 那正是用户把两地账号加混时最需要看到的
/// 一句话，所以文案里点明「不属于该站点」。
pub fn expired_message(region: Region, status: u16, site: bool) -> String {
    if site {
        format!(
            "ModelScope 令牌已失效或不属于该站点（{}），请重新粘贴（HTTP {status}）",
            region.site_base_url()
        )
    } else {
        format!("ModelScope 令牌已失效，请重新粘贴（HTTP {status}）")
    }
}
