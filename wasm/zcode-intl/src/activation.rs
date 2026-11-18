use super::{
    account, config,
    network::{self, ZCODE},
};
use agent2api_wasm_sdk::{json, Result, Value};

pub fn report(record: &Value) -> Result<Option<String>> {
    let secret = &record["secret"];
    let user = account::user(secret);
    if user.is_empty() {
        return Ok(None);
    }
    let mid = account::mid(secret);
    let version = config::app_version()?;
    let mut failure = None;
    for event in ["app_launch", "app_daily_active"] {
        account::unchanged(record)?;
        let body = json!({ "event_id": network::uuid()?, "client_timezone": "unknown", "client_language": "zh-CN",
            "element_name": event, "event_region": "app", "event_type": "view", "event_text": "",
            "event_extra_detail": {}, "user_id": user, "screen_resolution": "2560x1440",
            "app_version": version, "device_os_category": config::OS_CATEGORY, "device_os_version": "",
            "device_mid": mid, "mac_id": "", "marketing_params": "{}" });
        // 激活请求不带账号鉴权头，身份只在原生协议约定的请求体中。
        let result = network::request(
            "POST",
            &format!("{ZCODE}/api/v1/event/report"),
            json!({ "Content-Type": "application/json" }),
            Some(&body),
        )
        .and_then(|reply| network::envelope(reply, "激活事件上报"));
        if let Err(error) = result {
            if error.code.as_deref() == Some("forbidden") {
                return Err(error);
            }
            if failure.is_none() {
                failure = Some(error.message);
            }
        }
    }
    account::unchanged(record)?;
    Ok(failure)
}
