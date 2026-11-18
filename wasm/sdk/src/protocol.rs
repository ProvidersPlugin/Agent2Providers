use base64::Engine;

use crate::{host, json, Error, Result, Value};

pub const CAPABILITY: &str = "protocol.convert.v1";
const CHUNK_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

pub fn request(protocol: &str, body: &Value, model: &str) -> Result<Value> {
    host("protocol.request", json!({ "protocol": protocol, "body": body, "model": model }))
}

pub struct Converter {
    id: Option<String>,
}

impl Converter {
    pub fn new(protocol: &str, model: &str) -> Result<Self> {
        Self::open(json!({ "protocol": protocol, "model": model }))
    }

    pub fn with_session(protocol: &str, model: &str, session: &str) -> Result<Self> {
        Self::open(json!({ "protocol": protocol, "model": model, "session": session }))
    }

    fn open(params: Value) -> Result<Self> {
        let value = host("protocol.open", params)?;
        let id = value["id"].as_str().filter(|id| !id.is_empty())
            .ok_or_else(|| Error::new("协议转换句柄缺失"))?.to_string();
        Ok(Self { id: Some(id) })
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        if self.id.is_none() { return Err(Error::new("协议转换器已结束")); }
        for chunk in bytes.chunks(CHUNK_BYTES) {
            let converted = self.exchange(chunk, false)?;
            if output.len().saturating_add(converted.len()) > MAX_OUTPUT_BYTES {
                self.close();
                return Err(Error::coded("本次协议转换结果超过 4 MiB，请分次提交输入", "plugin_resource_limit"));
            }
            output.extend_from_slice(&converted);
        }
        Ok(output)
    }

    pub fn finish(&mut self) -> Result<Vec<u8>> {
        // 最后一片正文必须先 push；只以空片收尾，避免底层 flush 遗漏同时提交的正文。
        let result = self.exchange(&[], true);
        if result.is_ok() { self.id.take(); }
        result
    }

    fn exchange(&mut self, bytes: &[u8], finish: bool) -> Result<Vec<u8>> {
        let id = self.id.as_deref().ok_or_else(|| Error::new("协议转换器已结束"))?;
        let result = host("protocol.push", json!({ "id": id,
            "data": base64::engine::general_purpose::STANDARD.encode(bytes), "finish": finish }))
            .and_then(|value| {
                let data = value["data"].as_str().ok_or_else(|| Error::new("协议转换回应缺少字节内容"))?;
                if data.len() > MAX_OUTPUT_BYTES.div_ceil(3) * 4 {
                    return Err(Error::coded("协议转换回应超过 4 MiB", "plugin_resource_limit"));
                }
                let bytes = base64::engine::general_purpose::STANDARD.decode(data)
                    .map_err(|_| Error::new("协议转换回应编码无效"))?;
                if bytes.len() > MAX_OUTPUT_BYTES {
                    return Err(Error::coded("协议转换回应超过 4 MiB", "plugin_resource_limit"));
                }
                Ok(bytes)
            });
        if result.is_err() { self.close(); }
        result
    }

    fn close(&mut self) {
        if let Some(id) = self.id.take() { let _ = host("protocol.close", json!({ "id": id })); }
    }
}

impl Drop for Converter {
    fn drop(&mut self) { self.close(); }
}
