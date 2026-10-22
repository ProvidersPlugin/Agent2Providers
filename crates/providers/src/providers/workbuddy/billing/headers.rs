//! 计费接口的头集合（网关 `billing::request::{build_headers, whitelist_headers}`
//! 的插件侧副本）。
//!
//! ── 与转发头的关系（**不能互相替换**）──────────────────────────
//! 与 `workbuddy::auth::build_auth_headers` 的差别，逐条保留：
//!   - 计费版固定带 `Content-Type: application/json` 与 `Accept: application/json`
//!   - 计费版**不**带 `X-Department-Info`（转发需要，计费不需要）
//!   - 计费版 `X-User-Id` 缺失时给空串（转发版直接不给这个头）
//!   - 计费版无 Authorization 时也会给 `Bearer `（转发版跳过）
//!
//! 这些差别是上游两个 buildHeaders 各自的实现决定的，不是笔误。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::Value;

use crate::workbuddy_shared::endpoints::{resolve_edition, user_agent_for_edition};

/// 计费请求头（`session` 是账号会话形态：`auth` + `account` 两个子对象）。
///
/// `pub(crate)`：除计费自身（`billing::call`）外，`keepalive` 的国际版日活
/// 探测/领取也打同一批计费端点 —— 头集合必须同源，抄一份必然漂移。
pub(crate) fn build_headers(session: &Value, extra: &[(String, String)]) -> Vec<(String, String)> {
    let auth = session.get("auth").cloned().unwrap_or(Value::Null);
    let account = session.get("account").cloned().unwrap_or(Value::Null);
    let access_token = auth
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or("");

    let mut headers: Vec<(String, String)> = vec![
        ("Accept".to_string(), "application/json".to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
        (
            "Authorization".to_string(),
            format!("Bearer {access_token}"),
        ),
        (
            "X-User-Id".to_string(),
            account
                .get("uid")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
    ];
    // extra 在 Node 里是 `...extra` 插在 X-User-Id 之后的展开
    for (key, value) in extra {
        headers.push((key.clone(), value.clone()));
    }
    if let Some(enterprise_id) = account
        .get("enterpriseId")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        headers.push(("X-Enterprise-Id".to_string(), enterprise_id.to_string()));
        headers.push(("X-Tenant-Id".to_string(), enterprise_id.to_string()));
    }
    if let Some(domain) = auth
        .get("domain")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        headers.push(("X-Domain".to_string(), domain.to_string()));
    }
    headers
}

/// 运营活动接口需要的客户端白名单头（网关 `whitelist_headers` 的副本）。
///
/// 本插件目前没有调用方（活动 / 成长任务在 `growth` 里另有一套），但它是
/// 「按账号版本区分身份」这条口径的唯一定义 —— 保留给成长任务的将来接入。
#[allow(dead_code)]
pub(super) fn whitelist_headers(session: &Value) -> Vec<(String, String)> {
    let edition = session.get("edition").and_then(Value::as_str);
    let info = resolve_edition(edition);
    vec![
        ("User-Agent".to_string(), user_agent_for_edition(edition)),
        ("X-IDE-Type".to_string(), info.ua_platform.to_string()),
        ("X-IDE-Name".to_string(), info.product_name.to_string()),
        ("X-IDE-Version".to_string(), info.client_version.to_string()),
        ("X-Product".to_string(), info.product_name.to_string()),
    ]
}
