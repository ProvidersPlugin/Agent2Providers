//! MonkeyCode 出站请求的薄封装（Cookie 认证，统一超时与错误翻译）。
//!
//! ── 为什么收在一处 ──────────────────────────────────────────
//! 三处消费者（`login` 校验 / `models` 目录刷新）共用同一套
//! 超时、错误翻译与「业务码 → 人话」口径；各写一份会让「哪个码算登录失效」
//! 这类判据分叉。
//!
//! ── 业务码口径（参考 `mvp/auth.py` / `mvp/models.py` + `docs/05-api`）──
//! 上游是 HTTP 状态码 + `{code, msg, data}` 双轨：
//!   - 成功：`code == 0`（`{"code":0,"msg":"success","data":{…}}`）；
//!   - 未授权：HTTP 401 / 403，或 body 里 `code` 为 401 / 403
//!     （`{"code":401,"message":"未授权 [trace_id:…]"}`）；
//!   - 其余：业务失败，`message` / `msg` 是上游文案。
//! 本模块只判「HTTP 层失败」与「登录态失效」，业务码的**语义**由调用方各自
//! 翻译（登录校验与目录刷新要的处置不同）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use crate::http::send_raw;
use agent2api_provider_protocol::WireProxy as ResolvedProxy;
use crate::errors::GatewayError;
use crate::logging;

use super::endpoints;
use super::region::Region;

/// 请求超时（目录 / 校验都是短请求，20 秒足够；上游挂住时不要拖住网关）
const REQUEST_TIMEOUT_MS: u64 = 20_000;

/// 成功业务码（整数 `0`）
pub const SUCCESS_CODE: i64 = 0;

/// 读 body 里的业务码（整数；缺失时返回 None，调用方按「缺失即未知」处置）
pub fn business_code(payload: &Value) -> Option<i64> {
    payload.get("code").and_then(Value::as_i64)
}

/// 上游业务文案（`message` → `msg` 兜底）
pub fn upstream_message(payload: &Value) -> String {
    payload
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| payload.get("msg").and_then(Value::as_str))
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// 这个 HTTP 状态 / 业务码是不是「登录态失效」
pub fn is_auth_failure(status: u16, payload: &Value) -> bool {
    if status == 401 || status == 403 {
        return true;
    }
    matches!(business_code(payload), Some(401) | Some(403))
}

/// 发一次带 cookie 的请求（`method` 目前只有 GET；任务创建的专用请求在 `task.rs`）。
///
/// 返回上游响应体。HTTP 层失败 / 超时翻译成网关错误；登录态失效统一翻成
/// `401`（文案指向「重新粘贴 session」）。业务码**不在这里判** —— 调用方各有
/// 各的处置。
pub async fn request(
    method: &str,
    region: Region,
    path: &str,
    session: &str,
    body: Option<&Value>,
    what: &str,
) -> Result<Value, GatewayError> {
    if session.trim().is_empty() {
        return Err(GatewayError::with_status(
            401,
            "MonkeyCode 账号缺少 session，请重新粘贴登录态",
        ));
    }
    let url = format!("{}{path}", region.base_url());
    let headers = endpoints::authed_headers(region, session);
    let response = send_raw(method, &url, body, &headers, None, Some(REQUEST_TIMEOUT_MS))
        .await
        .map_err(|error| {
            if error.is_timeout() {
                GatewayError::with_status(504, format!("{what}超时，请稍后重试"))
            } else {
                GatewayError::with_status(502, format!("{what}失败：{error}"))
            }
        })?;
    let payload = response.payload.unwrap_or(Value::Null);
    if is_auth_failure(response.status, &payload) {
        return Err(GatewayError::with_status(
            401,
            format!(
                "{what}：MonkeyCode 登录态已失效，请重新粘贴 session（HTTP {}）",
                response.status
            ),
        ));
    }
    if !response.ok {
        let detail = upstream_message(&payload);
        let detail = if detail.is_empty() {
            String::new()
        } else {
            format!("：{detail}")
        };
        return Err(GatewayError::with_status(
            response.status as i32,
            format!("{what}返回 HTTP {}{detail}", response.status),
        ));
    }
    Ok(payload)
}

/// GET 便捷入口（校验 / 目录 / 任务列表共用）
pub async fn get_json(
    region: Region,
    path: &str,
    session: &str,
    what: &str,
) -> Result<Value, GatewayError> {
    request("GET", region, path, session, None, what).await
}

/// 停止一个上游任务（`PUT /api/v1/users/tasks/stop`，body `{"id": "<taskId>"}`）。
///
/// ── 为什么这件事必须做（本家最容易漏的一步）─────────────────
/// 上游的并发额度按「`pending` + `processing` 的任务数」算（业务码 10811），
/// 而**一轮对话结束不等于任务结束** —— `task-ended` 只结束本轮，任务仍停在
/// `processing`，那是给多轮复用留的窗口。本网关每个下游请求都新建一个任务，
/// 因此每次收尾都必须显式停止；否则账号槽位会被一直占着（直到 VM 空闲回收，
/// 默认 3 天），第二次请求就稳定撞 10811。
///
/// ── 失败不上升为错误 ────────────────────────────────────────
/// 调用点全在「这一轮已经定局」的收尾路径上（成功 / 出错 / 客户端断开 /
/// 超时）。此时再报错既改不了结论，又会让真正的失败原因被淹没。因此这里只
/// 返回 `bool` 表示是否停成，由调用方记一行 verbose；网络错误 / 401 / 业务码
/// 非 0 一律按「没停成」处理，**不抛错**。
///
/// `proxy` 与建任务用同一个出口：建任务走了账号代理，停止也必须走，
/// 否则「建得出去、停不回来」—— 那正好是槽位泄漏最坏的情形。
pub async fn stop_task(
    region: Region,
    session: &str,
    task_id: &str,
    proxy: Option<&ResolvedProxy>,
) -> bool {
    if task_id.trim().is_empty() || session.trim().is_empty() {
        return false;
    }
    let url = endpoints::task_stop_url(region);
    let headers = endpoints::authed_headers(region, session);
    let body = json!({ "id": task_id.trim() });
    let result = send_raw(
        "PUT",
        &url,
        Some(&body),
        &headers,
        proxy,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await;
    match result {
        Ok(response) => {
            // 成功形态是 `{"code":0,...}`；非 2xx 或业务码非 0 都算没停成
            let business_ok = match response.payload.as_ref() {
                Some(payload) => business_code(payload)
                    .map(|code| code == SUCCESS_CODE)
                    .unwrap_or(response.ok),
                None => response.ok,
            };
            let ok = response.ok && business_ok;
            logging::verbose(
                "[MonkeyCode]",
                &format!(
                    "停止任务 {task_id} {}（HTTP {}）",
                    if ok { "成功" } else { "未成功" },
                    response.status
                ),
            );
            ok
        }
        Err(error) => {
            logging::verbose(
                "[MonkeyCode]",
                &format!("停止任务 {task_id} 请求失败：{error}"),
            );
            false
        }
    }
}

/// 列出该账号**正在运行**（`pending` / `processing`）的任务 id。
///
/// 用于建任务撞 10811 之后的自愈：把残留任务停掉再重试一次。取不到
/// （网络错 / 结构变化）返回空表 —— 调用方据此放弃自愈，把原始错误透传。
pub async fn list_running_tasks(region: Region, session: &str) -> Vec<String> {
    let url = endpoints::tasks_running_url(region);
    let path = url
        .strip_prefix(&region.base_url())
        .unwrap_or(endpoints::TASKS_PATH);
    let payload = match get_json(region, path, session, "运行中任务列表查询").await {
        Ok(payload) => payload,
        Err(error) => {
            logging::verbose(
                "[MonkeyCode]",
                &format!("运行中任务列表查询失败（放弃自愈）：{}", error.message),
            );
            return Vec::new();
        }
    };
    let data = payload.get("data").unwrap_or(&payload);
    let tasks = data
        .get("tasks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut ids = Vec::new();
    for task in tasks {
        let status = task
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        // 只认这两种「占槽位」的状态（上游 `consts.TaskStatus*`）。上游已按
        // status 过滤，这里再判一次是因为过滤是上游的事：万一它忽略了 query
        // 参数，我们不能把 finished 的任务也去停一遍。
        if status != "pending" && status != "processing" {
            continue;
        }
        let id = task
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !id.is_empty() {
            ids.push(id.to_string());
        }
    }
    ids
}
