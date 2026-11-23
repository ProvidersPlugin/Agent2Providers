//! 主程序拥有的领取验证租约；不把账号、执行体或调度权限交给插件 JSON。
use serde::{Deserialize, Serialize};
use crate::{host, json, Error, Result};

#[derive(Clone, Copy, Serialize)]
pub struct StateSlot<'a> {
    pub conversation: &'a str,
    pub key: &'a str,
    pub schema: u32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseStatus {
    pub valid: bool,
    pub status: String,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub remaining_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lease {
    request_id: String,
    expires_at: i64,
    remaining_ms: u64,
    #[serde(skip)]
    closed: bool,
}

impl Lease {
    pub fn request_id(&self) -> &str { &self.request_id }
    pub fn expires_at(&self) -> i64 { self.expires_at }
    pub fn remaining_ms(&self) -> u64 { self.remaining_ms }
    pub fn check(&self) -> Result<LeaseStatus> { check(&self.request_id) }
    pub fn close(&mut self) -> Result<bool> {
        if self.closed { return Ok(false); }
        let closed = close(&self.request_id)?;
        self.closed = true;
        Ok(closed)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if !self.closed { let _ = close(&self.request_id); }
        // 陷阱/强制中断不保证 WASM Drop；宿主 Context 仍负责撤销与清理。
    }
}

pub fn open(plan_id: &str, config_id: &str, ttl_ms: u64, state: StateSlot<'_>) -> Result<Lease> {
    let lease: Lease = serde_json::from_value(host("verification.lease.open", json!({
        "planId": plan_id, "configId": config_id, "ttlMs": ttl_ms, "state": state,
    }))?).map_err(|_| Error::new("领取验证租约回应无效"))?;
    if lease.request_id.is_empty() || lease.request_id.len() > 128 || lease.expires_at <= 0
        || lease.remaining_ms == 0 || lease.remaining_ms > 50_000 {
        return Err(Error::new("领取验证租约边界无效"));
    }
    Ok(lease)
}

pub fn check(request_id: &str) -> Result<LeaseStatus> {
    serde_json::from_value(host("verification.lease.check", json!({ "requestId": request_id }))?)
        .map_err(|_| Error::new("领取验证租约状态无效"))
}

pub fn close(request_id: &str) -> Result<bool> {
    let result = host("verification.lease.close", json!({ "requestId": request_id }))?;
    result["closed"].as_bool().ok_or_else(|| Error::new("领取验证关闭回执无效"))
}
