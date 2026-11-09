use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const CAPABILITY: &str = "settings.schema.v1";
pub const OVERRIDES_METHOD: &str = "settings.overrides";

/// 分组显示名独立放在握手的 settings_group 中，旧宿主仍可按原规则解析 settings_schema。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    pub label: Value,
}

impl Group {
    pub fn new(label: impl Into<Value>) -> Self { Self { label: label.into() } }
}

/// 这里只声明非敏感用户选项；凭据继续走账号存储，不写入设置面板。
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
