//! 国际设备编号已有值优先；经独立授权后只读取国际客户端的固定设备文件。
//! 缺文件或未授权时生成一次性 UUID，按 flow/账号固定保存，不因失败更换。
//! 普通编号不是风控票据或硬件认证，不共享国内编号与缓存。
use super::util;
use agent2api_wasm_sdk::{json, Result, Value};

pub const UUID_NOTE: &str =
    "优先复用已有国际设备编号；授权后读取国际客户端固定设备文件，缺失时生成并保存一次性 UUID。国际兼容性未验收，不会因失败更换编号";
pub fn choose(existing: &str) -> Result<String> {
    util::identity(existing)?;
    if existing.is_empty() {
        match agent2api_wasm_sdk::client_device::machine_id(super::endpoints::PROVIDER)? {
            Some(value) => Ok(value),
            None => util::uuid(),
        }
    } else {
        Ok(existing.to_string())
    }
}
pub fn status() -> Value {
    json!({ "localReadAvailable": true, "requiresAuthorization": true, "default": "persisted_uuid", "defaultVerified": false,
        "message": UUID_NOTE, "nativeHelper": "desktop.device.qwenwork-intl.v1", "changesDeviceOnError": false })
}
