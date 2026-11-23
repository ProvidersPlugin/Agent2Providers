use crate::{json, native_call, Error, Result};

pub fn machine_id(provider: &str) -> Result<Option<String>> {
    let helper = format!("desktop.device.{provider}.v1");
    let value = match native_call(&helper, "read", json!({})) {
        Ok(value) => value,
        Err(error) if error.code.as_deref() == Some("native_helper_not_authorized") => return Ok(None),
        Err(error) => return Err(error),
    };
    if value["available"] == false { return Ok(None); }
    let text = value["machineId"].as_str().filter(|value| !value.is_empty() && value.len() <= 256
        && !value.contains('|') && !value.chars().any(|ch| ch.is_control() || ch.is_whitespace()))
        .ok_or_else(|| Error::new("本机设备编号格式无效"))?;
    Ok(Some(text.to_string()))
}
