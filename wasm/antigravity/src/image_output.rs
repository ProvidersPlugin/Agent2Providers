//! 图片SSE不经过会丢inlineData的Chat转换器。完整图片按块解码为当前调用资源。
//! 真实STOP+至少一张图片才成功；中途断线/错误不重发、未确认终态不返部分图片。

use super::{errors, sse::Reader, util};
use agent2api_wasm_sdk::{images::Resource, json, Error, Result, Value};
use base64::Engine;
use std::collections::BTreeSet;

struct Collector {
    images: Vec<Value>,
    hashes: BTreeSet<String>,
    usage: Option<Value>,
    finished: bool,
}
impl Collector {
    fn frame(&mut self, value: &Value) -> Result<()> {
        if errors::failed(value) || value.get("error").is_some_and(|e| !e.is_null()) {
            return Err(errors::upstream(200, value));
        }
        if errors::content_blocked(value) {
            return Err(errors::upstream(
                400,
                &json!({ "code": "antigravity_content_blocked" }),
            ));
        }
        let response = errors::inner(value);
        if let Some(metadata) = response
            .get("usageMetadata")
            .or_else(|| response.get("usage_metadata"))
            .filter(|m| m.is_object())
        {
            let input = metadata["promptTokenCount"].as_u64().unwrap_or(0);
            let output = metadata["candidatesTokenCount"]
                .as_u64()
                .unwrap_or(0)
                .saturating_add(metadata["thoughtsTokenCount"].as_u64().unwrap_or(0));
            self.usage = Some(json!({ "input_tokens": input, "output_tokens": output,
                "total_tokens": metadata["totalTokenCount"].as_u64().unwrap_or(input.saturating_add(output)) }));
        }
        for candidate in response["candidates"].as_array().into_iter().flatten() {
            if let Some(reason) = candidate["finishReason"].as_str().filter(|s| !s.is_empty()) {
                if reason == "STOP" {
                    self.finished = true;
                } else {
                    return Err(uncertain("图片没有正常完成，未返回不完整图片"));
                }
            }
            for part in candidate["content"]["parts"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if part["thought"] == true {
                    continue;
                }
                if let Some(inline) = part.get("inlineData").or_else(|| part.get("inline_data")) {
                    let data = inline["data"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| uncertain("图片数据为空"))?;
                    let mime = inline
                        .get("mimeType")
                        .or_else(|| inline.get("mime_type"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if !matches!(mime, "image/png" | "image/jpeg" | "image/webp") {
                        return Err(Error::http(
                            502,
                            "Antigravity 返回的图片格式超出当前主程序资源支持范围，未丢弃图片",
                        ));
                    }
                    let hash = util::digest(data.as_bytes());
                    if self.hashes.contains(&hash) {
                        continue;
                    }
                    if self.images.len() >= 16 {
                        return Err(Error::http(413, "Antigravity 图片数超过主程序资源上限")
                            .with_code("plugin_resource_limit"));
                    }
                    let resource = Resource::create(mime)?;
                    let count = data.len().div_ceil(64 * 1024);
                    for (index, chunk) in data.as_bytes().chunks(64 * 1024).enumerate() {
                        if index + 1 < count && chunk.contains(&b'=') {
                            return Err(uncertain("图片编码提前结束"));
                        }
                        let decoded = base64::engine::general_purpose::STANDARD
                            .decode(chunk)
                            .map_err(|_| uncertain("图片编码无效"))?;
                        resource.append(&decoded)?;
                    }
                    let mut result = resource.image_result()?;
                    result["mime_type"] = json!(mime);
                    self.hashes.insert(hash);
                    self.images.push(result);
                }
            }
        }
        Ok(())
    }
}

pub fn collect(mut reader: Reader) -> Result<Value> {
    let mut state = Collector {
        images: Vec::new(),
        hashes: BTreeSet::new(),
        usage: None,
        finished: false,
    };
    while let Some(event) = reader.next()? {
        if event.data.trim() == "[DONE]" {
            if !state.finished {
                return Err(uncertain("上游在图片完成前结束"));
            }
            break;
        }
        let value: Value =
            serde_json::from_str(&event.data).map_err(|_| uncertain("图片事件不是有效JSON"))?;
        drop(event.data);
        if !value.is_object() {
            return Err(uncertain("图片事件不是对象"));
        }
        if event.name == "error" {
            return Err(errors::upstream(200, &value));
        }
        state.frame(&value)?;
    }
    if !state.finished || state.images.is_empty() {
        return Err(uncertain("上游没有返回完整图片和正常结束标志"));
    }
    let mut result = json!({ "created": util::now()? / 1000, "data": state.images });
    if let Some(usage) = state.usage {
        result["usage"] = usage;
    }
    Ok(result)
}
pub fn uncertain(message: &str) -> Error {
    Error::http(
        502,
        format!("Antigravity 生图结果尚未确认：{message}；未自动重发，以免重复消耗额度"),
    )
    .with_code("image_output_uncertain")
}
