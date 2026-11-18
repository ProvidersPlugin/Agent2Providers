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

pub const CATEGORIES_METHOD: &str = "tasks.categories";

/// 分类单独返回，旧宿主仍能按原 tasks.list 解析；分类不授予账号或网络权限。
#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Checkin,
    Growth,
    Rewards,
    Maintenance,
    #[serde(other)]
    Other,
}

pub fn result(success: bool, message: &str) -> Value { json!({ "success": success, "message": message }) }
