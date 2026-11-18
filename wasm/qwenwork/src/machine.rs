//! 设备编号已有值优先；缺省先尝试经授权的固定文件读取，再生成一次性 UUID。
//! 登录 flow 和账号分别持久保存编号，后续不因请求失败重新生成。
//! 普通编号不是风控票据或硬件认证，不用于规避每设备限制。
use super::util;
use agent2api_wasm_sdk::{json, Result, Value};

pub const UUID_NOTE: &str =
    "优先复用已有设备编号；授权后读取官方固定设备文件，缺失时生成并保存一次性 UUID。兼容性未验收，不会因失败更换编号";
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
        "message": UUID_NOTE, "nativeHelper": "desktop.device.qwenwork.v1", "changesDeviceOnError": false })
}
