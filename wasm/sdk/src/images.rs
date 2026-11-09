use crate::{host, json, Result, Value};

pub struct Resource {
    pub id: String,
}

impl Resource {
    pub fn create(mime: &str) -> Result<Self> {
        let value = host("resource.create", json!({ "mime": mime }))?;
        Ok(Self { id: value["id"].as_str().ok_or_else(|| crate::Error::new("资源标识缺失"))?.into() })
    }
    pub fn append(&self, bytes: &[u8]) -> Result<()> {
        use base64::Engine;
        for chunk in bytes.chunks(64 * 1024) {
            host("resource.append", json!({ "id": self.id, "data": base64::engine::general_purpose::STANDARD.encode(chunk) }))?;
        }
        Ok(())
    }
    pub fn seal(&self) -> Result<Value> { host("resource.seal", json!({ "id": self.id })) }
    pub fn read(&self, offset: usize) -> Result<Vec<u8>> {
        use base64::Engine;
        let value = host("resource.read", json!({ "id": self.id, "offset": offset, "length": 64 * 1024 }))?;
        base64::engine::general_purpose::STANDARD.decode(value["data"].as_str().unwrap_or(""))
            .map_err(|_| crate::Error::new("资源编码无效"))
    }
    pub fn image_result(self) -> Result<Value> {
        self.seal()?;
        Ok(json!({ "resource": self.id }))
    }
    pub fn remove(self) -> Result<()> { host("resource.remove", json!({ "id": self.id })).map(|_| ()) }
}

pub fn response(images: Vec<Value>, usage: Value) -> Value {
    json!({ "created": crate::now() / 1000, "data": images, "usage": usage })
}
