mod abi;
pub mod catalog;
pub mod http;
pub mod images;
pub mod login;
pub mod settings;
pub mod sse;
pub mod state;
pub mod tasks;
pub mod websocket;

pub use abi::{allocate, dispatch};
pub use serde_json::{json, Value};

use serde::{Deserialize, Serialize};

pub const ABI_VERSION: i32 = 1;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Error {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_scope: Option<String>,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), code: None, status: None, upstream_code: None, reset_at: None, credential_scope: None }
    }
    pub fn coded(message: impl Into<String>, code: impl Into<String>) -> Self {
        Self { code: Some(code.into()), ..Self::new(message) }
    }
    pub fn unsupported() -> Self { Self::coded("插件不支持此操作", "unsupported") }
    pub fn http(status: u16, message: impl Into<String>) -> Self { Self { status: Some(status), ..Self::new(message) } }
    pub fn with_code(mut self, code: impl Into<String>) -> Self { self.code = Some(code.into()); self }
    pub fn with_reset(mut self, reset_at: Option<i64>) -> Self { self.reset_at = reset_at; self }
}

pub fn host(method: &str, params: Value) -> Result<Value> {
    abi::host(method, params)
}

pub fn now() -> i64 {
    host("clock.now", Value::Null).ok().and_then(|value| value.as_i64()).unwrap_or(0)
}

pub fn sleep(milliseconds: u64) -> Result<()> {
    host("clock.sleep", json!({ "milliseconds": milliseconds })).map(|_| ())
}

pub fn native_call(helper: &str, operation: &str, payload: Value) -> Result<Value> {
    host("native.call", json!({ "helper": helper, "operation": operation, "payload": payload }))
}

pub fn account() -> Result<Value> { host("account.get", json!({})) }

pub struct AccountLock(String);
impl AccountLock {
    pub fn acquire_named_conversation(conversation: &str) -> Result<Self> {
        let id = host("lock.acquire", json!({ "scope": "conversation", "conversation": conversation }))?;
        Ok(Self(id.as_str().ok_or_else(|| Error::new("会话锁回应无效"))?.to_string()))
    }
    pub fn acquire_conversation() -> Result<Self> {
        let id = host("lock.acquire", json!({ "scope": "conversation" }))?;
        Ok(Self(id.as_str().ok_or_else(|| Error::new("会话锁回应无效"))?.to_string()))
    }
    pub fn acquire() -> Result<Self> {
        let id = host("lock.acquire", json!({}))?;
        Ok(Self(id.as_str().ok_or_else(|| Error::new("账号锁回应无效"))?.to_string()))
    }
}
impl Drop for AccountLock {
    fn drop(&mut self) { let _ = host("lock.release", json!({ "id": self.0 })); }
}

pub fn session_get(id: &str) -> Result<Value> { host("session.get", json!({ "id": id })) }
pub fn session_put(id: &str, value: Value, ttl_ms: i64) -> Result<()> {
    host("session.put", json!({ "id": id, "value": value, "ttl_ms": ttl_ms })).map(|_| ())
}
pub fn session_remove(id: &str) -> Result<()> { host("session.remove", json!({ "id": id })).map(|_| ()) }

pub fn stream_head(status: u16) -> Result<()> {
    host("stream.head", json!({ "status": status, "headers": [["content-type", "text/event-stream"]] })).map(|_| ())
}
pub fn stream_bytes(bytes: &[u8]) -> Result<()> {
    use base64::Engine;
    for chunk in bytes.chunks(64 * 1024) {
        host("stream.chunk", json!({ "data": base64::engine::general_purpose::STANDARD.encode(chunk) }))?;
    }
    Ok(())
}
pub fn stream_event(value: &Value) -> Result<()> {
    stream_bytes(format!("data: {value}\n\n").as_bytes())
}

#[macro_export]
macro_rules! export_provider {
    ($handler:path) => {
        #[no_mangle]
        pub extern "C" fn a2a_abi_version() -> i32 { $crate::ABI_VERSION }
        #[no_mangle]
        pub extern "C" fn a2a_alloc(length: i32) -> i32 { $crate::allocate(length) }
        #[no_mangle]
        pub unsafe extern "C" fn a2a_call(pointer: i32, length: i32) -> i64 {
            $crate::dispatch(pointer, length, $handler)
        }
    };
}
