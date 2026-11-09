use crate::{host, json, Result, Value};

pub fn cached() -> Result<Value> { host("catalog.get", json!({})) }

pub fn refreshed(models: Vec<Value>) -> Value {
    json!({ "refreshed": true, "count": models.len(), "models": models,
        "meta": { "remote": true, "refreshed_at": crate::now() } })
}

pub fn unchanged(count: usize) -> Value { json!({ "refreshed": false, "count": count }) }

pub fn failed(message: &str) -> Value { json!({ "refreshed": false, "message": message }) }
