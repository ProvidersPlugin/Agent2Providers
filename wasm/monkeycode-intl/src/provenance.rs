//! 来源声明只接受已确认国际值。没有域信息的裸 Session／Cookie 无法离线判地区。
//! 这些输入标记仅作拒绝跨区的约束，不保存 region、不选择端点。

use crate::{endpoints, util};
use agent2api_wasm_sdk::{Error, Result, Value};

pub fn declaration(key: &str, value: &Value) -> Result<()> {
    let key = key.replace('_', "").to_ascii_lowercase();
    if ![
        "provider",
        "providerid",
        "edition",
        "region",
        "baseurl",
        "apibase",
        "origin",
        "domain",
        "host",
        "hostname",
        "endpoint",
    ]
    .contains(&key.as_str())
    {
        return Ok(());
    }
    let text = util::optional_text(value)?.ok_or_else(invalid)?;
    let valid = match key.as_str() {
        "provider" | "providerid" | "edition" => text == endpoints::ID,
        "region" => text == "intl",
        "domain" | "host" | "hostname" => text == endpoints::DOMAIN,
        _ => text == endpoints::ORIGIN || text == format!("{}/", endpoints::ORIGIN),
    };
    if valid {
        Ok(())
    } else {
        Err(invalid())
    }
}

pub fn check(value: &Value) -> Result<()> {
    let Some(fields) = value.as_object() else {
        return Ok(());
    };
    for (key, value) in fields {
        declaration(key, value)?;
    }
    if fields
        .get("desktop")
        .is_some_and(|value| !value.is_null() && value != &Value::Bool(false))
    {
        return Err(Error::http(400, "MonkeyCode 国际本批不导入本机或桌面引用"));
    }
    Ok(())
}

/// 只用于调用方的手动载荷/凭据；不扫描宿主代理配置，也不用于上游模型 provider 字段。
pub fn nested(value: &Value, depth: usize) -> Result<()> {
    if depth > 16 {
        return Err(invalid());
    }
    match value {
        Value::Object(fields) => {
            check(value)?;
            for value in fields.values() {
                nested(value, depth + 1)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                nested(value, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn invalid() -> Error {
    Error::http(
        400,
        "凭据声明的来源不是已确认的 MonkeyCode 国际站，未发送请求或尝试其它站点",
    )
    .with_code("monkeycode_intl_source_mismatch")
}
