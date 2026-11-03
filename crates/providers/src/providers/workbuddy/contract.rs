//! 请求头按官方链路分组：对话、请求框架、匿名登录与网页 Cookie 不混用。

use super::{auth, region::Region};
use serde_json::Value;

pub(super) fn framework_headers(
    session: &Value,
    region: Region,
    daemon: bool,
) -> Vec<(String, String)> {
    let edition = region.edition();
    let mut headers = vec![
        ("Accept".into(), "application/json, text/plain, */*".into()),
        ("X-Requested-With".into(), "XMLHttpRequest".into()),
        ("X-Request-ID".into(), crate::ids::new_message_id()),
        ("X-Product".into(), "SaaS".into()),
        (
            "User-Agent".into(),
            if daemon {
                crate::workbuddy_shared::endpoints::user_agent_for_edition(Some(region.id()))
            } else {
                edition.chat_ua.to_string()
            },
        ),
    ];
    headers.extend(auth::build_auth_headers(session));
    headers
}

pub(super) fn login_headers(region: Region, authority: &str, state: bool) -> Vec<(String, String)> {
    let mut headers = framework_headers(&serde_json::json!({}), region, true);
    for name in [
        "X-No-Authorization",
        "X-No-User-Id",
        "X-No-Enterprise-Id",
        "X-No-Department-Info",
    ] {
        headers.push((name.into(), "true".into()));
    }
    if state {
        headers.push(("X-Domain".into(), authority.to_string()));
        headers.push(("X-Custom-Channel-Id".into(), "unknown".into()));
    }
    headers
}

pub(super) fn proxy_of(
    record: &Value,
) -> Result<Option<agent2api_provider_protocol::WireProxy>, crate::errors::GatewayError> {
    match crate::proxy_util::resolve_account_proxy(record.get("proxy")) {
        Some(crate::proxy_util::ProxyResolution::Resolved(proxy)) => Ok(Some(proxy)),
        Some(crate::proxy_util::ProxyResolution::Failed(message)) => {
            Err(crate::errors::GatewayError::with_status(502, message))
        }
        None => Ok(None),
    }
}

pub(super) fn web_origin(region: Region) -> &'static str {
    match region {
        Region::Cn => "https://www.workbuddy.cn",
        Region::Intl => "https://www.workbuddy.ai",
    }
}

pub(super) fn network_policy(region: Region) -> Value {
    if region != Region::Cn {
        return Value::Null;
    }
    serde_json::json!({
        "trustedOrigins": ["https://www.workbuddy.cn", "https://copilot.tencent.com", "https://wb.tencentbuddy.com"],
        "backupOrigin": "https://wb.tencentbuddy.com",
        "retryStatuses": [408, 502, 503, 504], "addressStatuses": [408, 502, 503],
        "httpDns": { "token": "719980335", "bootstrapIps": ["119.29.29.99", "119.28.28.99"] },
        "fallbackAddresses": ["162.14.3.241"], "maxAttempts": 4,
    })
}
