//! 计费、签到与活跃请求共用官方后台请求框架头；不混入对话专属追踪头。

use serde_json::Value;

use crate::workbuddy_shared::endpoints::{resolve_edition, user_agent_for_edition};

/// 计费请求头（`session` 是账号会话形态：`auth` + `account` 两个子对象）。
///
/// `pub(crate)`：除计费自身（`billing::call`）外，`keepalive` 的国际版日活
/// 探测/领取也打同一批计费端点 —— 头集合必须同源，抄一份必然漂移。
pub(crate) fn build_headers(session: &Value, extra: &[(String, String)]) -> Vec<(String, String)> {
    let region = super::super::Region::from_edition_id(session.get("edition").and_then(Value::as_str));
    let mut headers = super::super::contract::framework_headers(session, region, true);
    for (key, value) in extra {
        if let Some((_, current)) = headers.iter_mut().find(|(name, _)| name.eq_ignore_ascii_case(key)) {
            *current = value.clone();
        } else { headers.push((key.clone(), value.clone())); }
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
