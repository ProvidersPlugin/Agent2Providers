use crate::{host, json, Error, Result, Value};

pub struct WebSocket { id: String }

impl WebSocket {
    pub fn open(url: &str, headers: Value, protocols: &[String]) -> Result<Self> {
        let value = host("ws.open", json!({ "url": url, "headers": headers, "protocols": protocols }))?;
        Ok(Self { id: value["id"].as_str().ok_or_else(|| Error::new("长连接标识缺失"))?.into() })
    }
    pub fn read(&self) -> Result<Value> { host("ws.read", json!({ "id": self.id })) }
    pub fn send_text(&self, text: &str) -> Result<()> {
        host("ws.send", json!({ "id": self.id, "type": "text", "data": text })).map(|_| ())
    }
    pub fn send_bytes(&self, bytes: &[u8]) -> Result<()> {
        use base64::Engine;
        host("ws.send", json!({ "id": self.id, "type": "binary",
            "data": base64::engine::general_purpose::STANDARD.encode(bytes) })).map(|_| ())
    }
}
impl Drop for WebSocket {
    fn drop(&mut self) { let _ = host("ws.close", json!({ "id": self.id })); }
}
