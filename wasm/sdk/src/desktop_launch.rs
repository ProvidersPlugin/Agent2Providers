use serde::{Deserialize, Serialize};

use crate::{json, Error, Result, Value};

pub const CAPABILITY: &str = "desktop.launch.v1";
pub const CHECK: &str = "desktop.launch.check";
pub const PREPARE: &str = "desktop.launch.prepare";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientSummary {
    pub id: String,
    pub label: String,
    pub platform: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialPlan {
    pub version: u32,
    pub client: String,
    pub expect: Value,
    pub changes: Vec<Change>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub resource: String,
    pub value: Value,
}

pub fn context() -> Result<Value> { crate::host("desktop.launch.context", json!({})) }

pub fn existing(resource: &str) -> Result<Value> {
    crate::host("desktop.launch.read", json!({ "resource": resource }))
}

pub fn ready(available: bool, reason: &str) -> Value {
    json!({ "available": available, "reason": reason })
}

pub fn plan(client: &str, expect: Value, changes: Vec<Change>) -> Result<Value> {
    serde_json::to_value(CredentialPlan { version: 1, client: client.into(), expect, changes })
        .map_err(|_| Error::new("本机凭证准备结果无法编码"))
}
