use serde::{Deserialize, Serialize};
use crate::{host, json, Result, Value};

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope { Login, Conversation, Account }

#[derive(Clone, Debug, Deserialize)]
pub struct Snapshot {
    pub revision: u64,
    pub schema: u32,
    pub value: Value,
    #[serde(default)]
    pub compatible: bool,
}

pub fn conversation_get(conversation: &str, key: &str, schema: u32) -> Result<Snapshot> {
    serde_json::from_value(host("state.get", json!({ "scope": "conversation", "conversation": conversation,
        "key": key, "schema": schema }))?).map_err(|_| crate::Error::new("状态快照格式无效"))
}

pub fn conversation_compare_exchange(conversation: &str, key: &str, schema: u32, revision: u64, value: Value, ttl_ms: i64) -> Result<bool> {
    let result = host("state.compare_exchange", json!({ "scope": "conversation", "conversation": conversation,
        "key": key, "schema": schema, "revision": revision, "value": value, "ttl_ms": ttl_ms }))?;
    Ok(result["updated"] == true)
}

pub fn get(scope: Scope, key: &str, schema: u32) -> Result<Snapshot> {
    serde_json::from_value(host("state.get", json!({ "scope": scope, "key": key, "schema": schema }))?)
        .map_err(|_| crate::Error::new("状态快照格式无效"))
}

pub fn compare_exchange(scope: Scope, key: &str, schema: u32, revision: u64, value: Value, ttl_ms: i64) -> Result<bool> {
    let result = host("state.compare_exchange", json!({ "scope": scope, "key": key,
        "schema": schema, "revision": revision, "value": value, "ttl_ms": ttl_ms }))?;
    Ok(result["updated"] == true)
}
