use crate::{self as sdk, json, Error, Result, Value};

pub fn status(provider: &str) -> Result<Value> {
    sdk::native_call(&format!("desktop.refresh.{provider}.v1"), "status", json!({}))
}

pub fn refresh(provider: &str) -> Result<Value> {
    let current = status(provider)?;
    if current["authorized"] != true {
        return Err(Error::http(403, "请在插件设置中授权本机续期与安全写回")
            .with_code("native_helper_not_authorized"));
    }
    sdk::native_call(&format!("desktop.refresh.{provider}.v1"), "refresh",
        json!({ "sourceRevision": current["sourceRevision"] }))
}
