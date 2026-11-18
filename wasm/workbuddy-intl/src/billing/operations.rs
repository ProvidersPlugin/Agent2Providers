//! 国际运营只读接口；普通每日签到与国内gift/compensation不属于国际操作。
use crate::{endpoints, headers, network, session::Session, util};
use agent2api_wasm_sdk::{json, Result, Value};

pub fn banner(current: &Session) -> Result<Value> {
    let mut h = headers::framework(&current.secret)?;
    h["X-IDE-Type"] = json!("WorkBuddy");
    h["X-IDE-Name"] = json!("WorkBuddy AI");
    h["X-IDE-Version"] = json!(endpoints::VERSION);
    h["X-Product"] = json!("WorkBuddy AI");
    let raw = network::request(
        "GET",
        &current.endpoint()?,
        "/v2/activity/workbuddy/banner",
        h,
        None,
        &current.secrets(),
    )?;
    if raw.is_null() {
        return Ok(Value::Null);
    }
    let mut value = json!({ "id": raw["activity_id"], "active": util::truth(&raw["activity_online_status"]) && raw["status"] == "None",
        "bannerContent": raw["banner_content"], "link": raw["detail_link"], "level": raw["activity_level"],
        "startTime": raw["start_time"], "endTime": raw["end_time"] });
    if matches!(
        raw["action"]["type"].as_str(),
        Some("open_url" | "switch_model")
    ) {
        let mut action = json!({ "type": raw["action"]["type"], "label": raw["action"]["label"] });
        for (from, to) in [
            ("url", "url"),
            ("model_id", "modelId"),
            ("fallback_url", "fallbackUrl"),
        ] {
            if let Some(v) = raw["action"].get(from) {
                action[to] = v.clone();
            }
        }
        value["action"] = action;
    }
    Ok(value)
}
pub fn ambassador(current: &Session) -> Result<Value> {
    let raw = super::request(current, "GET", "/v2/activity/ambassador/status", None)?;
    Ok(if raw.is_null() {
        Value::Null
    } else {
        json!({ "isAmbassador": util::truth(&raw["isAmbassador"]) })
    })
}
