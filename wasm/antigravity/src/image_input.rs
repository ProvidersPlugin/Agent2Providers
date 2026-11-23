//! 图片输入只生成已确认 inlineData 形状。公网图片用宿主受控无凭证下载；不静默丢图。
//! 大Base64入站仍受主程序16MiB消息上限；本包能资源化出站，不能扩大入站ABI。

use agent2api_wasm_sdk::{images::Resource, json, Error, Result, Value};
use base64::Engine;

const MAX_IMAGE_BYTES: usize = 24 * 1024 * 1024;

fn too_large() -> Error {
    Error::http(
        413,
        "Antigravity 图片超过当前 WASM 内联承载上限，未丢弃该图片；需要主程序入站资源化支持",
    )
    .with_code("plugin_resource_limit")
}
fn mime(raw: &str) -> Result<&str> {
    if matches!(raw, "image/png" | "image/jpeg" | "image/webp" | "image/gif") {
        Ok(raw)
    } else {
        Err(Error::http(
            400,
            "Antigravity 参考图片类型无效，请使用 PNG、JPEG、WebP 或 GIF",
        ))
    }
}

pub fn data_uri(source: &str) -> Result<Value> {
    let (meta, data) = source
        .trim()
        .strip_prefix("data:")
        .and_then(|s| s.split_once(','))
        .ok_or_else(|| Error::http(400, "Antigravity 图片缺少有效 data URI"))?;
    let kind = meta
        .strip_suffix(";base64")
        .ok_or_else(|| Error::http(400, "Antigravity 图片必须使用 Base64 编码"))?;
    let kind = mime(if kind.is_empty() { "image/png" } else { kind })?;
    if data.is_empty() {
        return Err(Error::http(400, "Antigravity 图片为空"));
    }
    if data.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
        return Err(too_large());
    }
    // 分片校验，不额外分配整幅解码图；只允许最后一片带填充。
    validate_base64(data)?;
    Ok(json!({ "inlineData": { "mimeType": kind, "data": data } }))
}

pub fn validate_base64(data: &str) -> Result<()> {
    let chunks = data.len().div_ceil(64 * 1024);
    for (index, chunk) in data.as_bytes().chunks(64 * 1024).enumerate() {
        if index + 1 < chunks && chunk.contains(&b'=') {
            return Err(Error::http(
                400,
                "Antigravity 图片Base64提前结束，未跳过该图片",
            ));
        }
        base64::engine::general_purpose::STANDARD
            .decode(chunk)
            .map_err(|_| Error::http(400, "Antigravity 图片Base64编码无效，未跳过该图片"))?;
    }
    Ok(())
}

pub fn source(part: &Value) -> Option<&str> {
    match part.get("image_url") {
        Some(Value::String(url)) => Some(url),
        Some(other) => other["url"].as_str(),
        None => part["url"].as_str(),
    }
}

pub fn from_part(part: &Value) -> Result<Value> {
    let source = source(part).ok_or_else(|| Error::http(400, "Antigravity 图片块缺少图片地址"))?;
    if source.trim().starts_with("data:") {
        return data_uri(source);
    }
    if !source.starts_with("https://") && !source.starts_with("http://") {
        return Err(Error::http(
            400,
            "Antigravity 图片只接受内联数据或公网图片URL，不读取本机路径",
        ));
    }
    let resource = Resource::fetch_public(source)?;
    let result = (|| {
        let meta = resource.seal()?;
        let size = meta["size"]
            .as_u64()
            .ok_or_else(|| Error::http(502, "公共图片资源缺少长度"))? as usize;
        if size > MAX_IMAGE_BYTES {
            return Err(too_large());
        }
        let kind = mime(meta["mime"].as_str().unwrap_or(""))?;
        let mut encoded = String::with_capacity(size.div_ceil(3) * 4);
        let mut offset = 0;
        let mut rest = Vec::new();
        while offset < size {
            let chunk = resource.read(offset)?;
            if chunk.is_empty() {
                return Err(Error::http(502, "公共图片资源不完整"));
            }
            offset = offset.saturating_add(chunk.len());
            if offset > size {
                return Err(Error::http(502, "公共图片资源长度变化"));
            }
            rest.extend_from_slice(&chunk);
            let aligned = rest.len() / 3 * 3;
            base64::engine::general_purpose::STANDARD.encode_string(&rest[..aligned], &mut encoded);
            rest.drain(..aligned);
        }
        base64::engine::general_purpose::STANDARD.encode_string(&rest, &mut encoded);
        let mut part = json!({ "inlineData": { "mimeType": kind } });
        part["inlineData"]["data"] = Value::String(encoded);
        Ok(part)
    })();
    let _ = resource.remove();
    result
}
