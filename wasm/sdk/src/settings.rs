use serde::{Deserialize, Serialize};
use crate::{Result, Value};

pub const CAPABILITY: &str = "settings.schema.v1";
pub const OVERRIDES_METHOD: &str = "settings.overrides";

/// 与 settings_schema 同级的 settings_group；只描述名称，不指定宿主页面或布局。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    pub label: Value,
}

impl Group {
    pub fn new(label: impl Into<Value>) -> Self { Self { label: label.into() } }
}

/// 随 init 元数据的 settings_schema 返回；用户覆盖值由主程序保存。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Schema {
    pub version: u32,
    pub title: Value,
    #[serde(default)]
    pub description: Value,
    #[serde(default)]
    pub storage_provider: Option<String>,
    #[serde(default)]
    pub runtime_overrides: bool,
    pub fields: Vec<Field>,
}

impl Schema {
    pub fn new(title: impl Into<Value>, fields: Vec<Field>) -> Self {
        Self { version: 1, title: title.into(), description: Value::Null, storage_provider: None,
            runtime_overrides: false, fields }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub key: String,
    pub label: Value,
    #[serde(default)]
    pub description: Value,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub default: Value,
    #[serde(default)]
    pub trim: bool,
    #[serde(default)]
    pub empty_is_default: bool,
    #[serde(default)]
    pub min_length: Option<usize>,
    #[serde(default)]
    pub max_length: Option<usize>,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub minimum: Option<f64>,
    #[serde(default)]
    pub maximum: Option<f64>,
    #[serde(default)]
    pub options: Vec<Choice>,
}

impl Field {
    pub fn new(key: impl Into<String>, label: impl Into<Value>, kind: impl Into<String>, default: Value) -> Self {
        Self { key: key.into(), label: label.into(), kind: kind.into(), default, description: Value::Null,
            trim: false, empty_is_default: false, min_length: None, max_length: None, pattern: None,
            minimum: None, maximum: None, options: Vec::new() }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub value: Value,
    pub label: Value,
}

pub fn get(key: &str) -> Result<Value> { crate::host("settings.get", crate::json!({ "key": key })) }
