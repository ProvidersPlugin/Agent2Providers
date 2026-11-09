use serde::{Deserialize, Serialize};
use crate::{json, Value};

#[derive(Clone, Deserialize, Serialize)]
pub struct Task {
    pub id: String,
    pub label: String,
    pub enabled: bool,
    #[serde(default)]
    pub times: Vec<String>,
    #[serde(default)]
    pub interval_ms: Option<i64>,
    pub utc_offset_minutes: i32,
    #[serde(default)]
    pub checkin: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_config: Option<String>,
}

pub fn result(success: bool, message: &str) -> Value { json!({ "success": success, "message": message }) }
