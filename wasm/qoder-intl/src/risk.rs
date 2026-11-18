//! 设备验证只调用已授权的固定宿主组件，不接收路径、命令或自填设备票据。
use super::account::{self, Snapshot};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub const NOT_READY: &str = "qoder_umid_unavailable";
pub const NOTE: &str = "Qoder 国际活动需要已授权的官方设备验证组件；设备每日限领和账号资格仍由上游决定";
const HELPER: &str = "desktop.qoder-umid.v1";

pub fn unavailable() -> Error { Error::http(503, NOTE).with_code(NOT_READY) }
pub fn augment(snapshot: &Snapshot, list: &Value) -> Result<Value> {
    account::unchanged(snapshot)?;
    let uid = list["uid"].as_str().filter(|uid| !uid.is_empty()).ok_or_else(||
        Error::http(502, "活动清单缺少有效用户标识").with_code("qoder_campaign_uid_missing"))?;
    let result = sdk::native_call(HELPER, "identity", json!({ "uid": uid }))?;
    if result["uid"] != uid || !result["headers"].is_object() { return Err(unavailable()); }
    let mut headers = super::client::sash_headers(&snapshot.credentials.access);
    for (name, value) in result["headers"].as_object().into_iter().flatten() {
        headers[name] = value.clone();
    }
    account::unchanged(snapshot)?;
    Ok(headers)
}
pub fn blocked(error: &Error) -> bool {
    matches!(error.code.as_deref(), Some(NOT_READY | "native_helper_not_authorized" | "qoder_campaign_uid_missing"))
}
pub fn status() -> Result<Value> { sdk::native_call(HELPER, "status", json!({})) }
pub fn install() -> Result<Value> { sdk::native_call(HELPER, "install", json!({})) }
