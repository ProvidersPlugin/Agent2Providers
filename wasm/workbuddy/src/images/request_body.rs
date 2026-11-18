//! 编辑请求只借用原data URL，逐片写完整JSON资源；不重新生成大型图片数组或base64信封。
//! 资源普通返回路径显式清理，调用中断由宿主Context回收；没有提交重试。

use std::io::{self, Write};

use agent2api_wasm_sdk::{http, images::Resource, Error, Result, Value};
use base64::Engine;

use super::{mime, network, uncertain};

const CHUNK: usize = 64 * 1024;
const DECODE_CHUNK: usize = CHUNK / 4 * 3;
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_INLINE_BYTES: usize = 14 * 1024 * 1024;
// 对齐现有调用资源预算，不提高标准编辑入口的16/10/14MiB限制。
const MAX_RESOURCE_BYTES: usize = 96 * 1024 * 1024;

pub(super) struct References<'a> {
    images: &'a [Value],
}

impl<'a> References<'a> {
    pub(super) fn parse(value: &'a Value) -> Result<Self> {
        let images = value.as_array().filter(|images| !images.is_empty() && images.len() <= 16)
            .ok_or_else(|| Error::http(400, "参考图需为1至16张"))?;
        let mut inline_bytes = 0usize;
        for image in images {
            let url = image_url(image)?;
            inline_bytes = inline_bytes.checked_add(url.len())
                .filter(|total| *total <= MAX_INLINE_BYTES)
                .ok_or_else(|| Error::http(413, "参考图内联数据总大小不得超过14MiB"))?;
            validate(url)?;
        }
        Ok(Self { images })
    }

    pub(super) fn open(self, origin: &str, headers: Value, body: &Value) -> Result<http::Response> {
        let fields = body.as_object().filter(|fields| fields.get("image") == Some(&Value::Null))
            .ok_or_else(|| Error::http(400, "WorkBuddy图片编辑请求结构无效，未提交请求"))?;
        let resource = Resource::create("application/octet-stream")
            .map_err(|error| preparation_error(error, "WorkBuddy图片请求资源创建失败，未提交请求"))?;
        let prepared = self.prepare(&resource, fields);
        if let Err(error) = prepared {
            // append/序列化/seal任何失败都只清理，绝不调用网络入口。
            return match resource.remove() {
                Ok(()) => Err(error),
                Err(_) => Err(Error::http(503, "WorkBuddy图片请求准备失败且资源清理未确认，未提交请求")
                    .with_code("image_request_cleanup_unconfirmed")),
            };
        }
        // 仅封存完整正文后发送一次；原地址验证与Content-Type由network保留。
        let response = network::open_edit_resource(origin, headers, &resource);
        // raw已按现有宿主契约交给HTTP句柄；先释放请求槽，再允许创建最多16个结果资源。
        let removed = resource.remove();
        if removed.is_err() {
            // 若已经取得响应，Drop关闭它；不带着未确认占用的槽继续生成不完整结果。
            drop(response);
            let mut error = uncertain();
            error.message = "WorkBuddy图片请求资源清理未确认，结果未知；未重新提交请求".into();
            return Err(error);
        }
        response.map_err(|_| uncertain())
    }

    fn prepare(&self, resource: &Resource, fields: &serde_json::Map<String, Value>) -> Result<()> {
        let mut writer = ResourceWriter::new(resource)?;
        if self.write_json(&mut writer, fields).is_err() {
            return Err(writer.failure.take().unwrap_or_else(||
                Error::http(400, "WorkBuddy图片请求JSON序列化失败，未提交请求")));
        }
        let size = writer.total;
        drop(writer);
        let sealed = resource.seal()
            .map_err(|error| preparation_error(error, "WorkBuddy图片请求资源封存失败，未提交请求"))?;
        if sealed["id"].as_str() != Some(resource.id.as_str())
            || sealed["size"].as_u64() != u64::try_from(size).ok()
            || sealed["mime"].as_str() != Some("application/octet-stream") {
            return Err(Error::http(502, "WorkBuddy图片请求资源封存回执不一致，未提交请求"));
        }
        Ok(())
    }

    fn write_json(&self, writer: &mut ResourceWriter<'_>, fields: &serde_json::Map<String, Value>) -> io::Result<()> {
        writer.write_all(b"{")?;
        for (index, (key, value)) in fields.iter().enumerate() {
            if index != 0 { writer.write_all(b",")?; }
            serde_json::to_writer(&mut *writer, key).map_err(json_error)?;
            writer.write_all(b":")?;
            if key == "image" {
                writer.write_all(b"[")?;
                for (index, image) in self.images.iter().enumerate() {
                    if index != 0 { writer.write_all(b",")?; }
                    let url = image["image_url"].as_str().ok_or_else(|| io::Error::other("参考图结构无效"))?;
                    // serde_json负责字符串转义；只借用url，绝不to_string整张图或整份正文。
                    serde_json::to_writer(&mut *writer, url).map_err(json_error)?;
                }
                writer.write_all(b"]")?;
            } else {
                serde_json::to_writer(&mut *writer, value).map_err(json_error)?;
            }
        }
        writer.write_all(b"}")?;
        writer.flush()
    }
}

fn image_url(image: &Value) -> Result<&str> {
    image["image_url"].as_str().ok_or_else(|| Error::http(400, "参考图缺少内联地址"))
}

fn validate(url: &str) -> Result<()> {
    let (declared, encoded) = url.strip_prefix("data:").and_then(|text| text.split_once(";base64,"))
        .ok_or_else(|| Error::http(400, "参考图必须是内联data URL"))?;
    if encoded.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
        return Err(Error::http(413, "单张参考图不得超过10MiB"));
    }
    let mut buffer = [0u8; DECODE_CHUNK];
    let mut header = [0u8; 12];
    let mut header_len = 0usize;
    let mut total = 0usize;
    let mut chunks = encoded.as_bytes().chunks(CHUNK).peekable();
    while let Some(chunk) = chunks.next() {
        // 块长是4的倍数，只有整串最后一块允许padding；不能接受拼接的多个base64串。
        if chunks.peek().is_some() && chunk.contains(&b'=') {
            return Err(Error::http(400, "参考图编码无效"));
        }
        let length = base64::engine::general_purpose::STANDARD.decode_slice(chunk, &mut buffer)
            .map_err(|_| Error::http(400, "参考图编码无效"))?;
        total = total.checked_add(length).filter(|total| *total <= MAX_IMAGE_BYTES)
            .ok_or_else(|| Error::http(413, "单张参考图不得超过10MiB"))?;
        let keep = header.len().saturating_sub(header_len).min(length);
        header[header_len..header_len + keep].copy_from_slice(&buffer[..keep]);
        header_len += keep;
    }
    let actual = mime(&header[..header_len])
        .ok_or_else(|| Error::http(400, "参考图必须是PNG、JPEG或WebP"))?;
    if actual != declared && !(actual == "image/jpeg" && declared == "image/jpg") {
        return Err(Error::http(400, "参考图声明格式与实际内容不符"));
    }
    Ok(())
}

fn preparation_error(mut error: Error, message: &str) -> Error {
    // 不回显资源ID、账号或原始错误；保留宿主取消/超时等错误分类。
    error.message = message.into();
    error
}

fn json_error(_: serde_json::Error) -> io::Error {
    io::Error::other("图片请求JSON序列化失败")
}

struct ResourceWriter<'a> {
    resource: &'a Resource,
    buffer: Vec<u8>,
    total: usize,
    failure: Option<Error>,
}

impl<'a> ResourceWriter<'a> {
    fn new(resource: &'a Resource) -> Result<Self> {
        let mut buffer = Vec::new();
        buffer.try_reserve_exact(CHUNK)
            .map_err(|_| Error::http(503, "WorkBuddy图片请求分片缓冲不可用，未提交请求"))?;
        Ok(Self { resource, buffer, total: 0, failure: None })
    }

    fn failed(&mut self, error: Error) -> io::Error {
        self.failure.get_or_insert(error);
        io::Error::other("图片请求资源写入失败")
    }
}

impl Write for ResourceWriter<'_> {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        if self.failure.is_some() { return Err(io::Error::other("图片请求资源已停止写入")); }
        let length = bytes.len();
        let Some(total) = self.total.checked_add(length).filter(|total| *total <= MAX_RESOURCE_BYTES) else {
            return Err(self.failed(Error::http(413, "WorkBuddy图片请求超过96MiB调用资源上限，未提交请求")));
        };
        while !bytes.is_empty() {
            let length = CHUNK.saturating_sub(self.buffer.len()).min(bytes.len());
            self.buffer.extend_from_slice(&bytes[..length]);
            bytes = &bytes[length..];
            if self.buffer.len() == CHUNK { self.flush()?; }
        }
        self.total = total;
        Ok(length)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.failure.is_some() { return Err(io::Error::other("图片请求资源已停止写入")); }
        if !self.buffer.is_empty() {
            if let Err(error) = self.resource.append(&self.buffer) {
                return Err(self.failed(preparation_error(error, "WorkBuddy图片请求分片保存失败，未提交请求")));
            }
            self.buffer.clear();
        }
        Ok(())
    }
}
