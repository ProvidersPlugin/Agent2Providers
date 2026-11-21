use base64::Engine;
use std::io::{self, Write};

use crate::{host, json, Error, Result, Value};

pub struct Response {
    pub status: u16,
    pub headers: Value,
    pub headers_raw: Vec<(String, String)>,
    id: String,
    done: bool,
}

/// 大正文的发送通道：先分片写入当前调用资源，再由宿主直接取原字节发出。
///
/// 普通内联发送会把整份正文变成字符串并做 Base64 编码，峰值内存占用数倍于原文；
/// 大于阈值的正文自动走本通道（旧声明缺少 `resources.v1` 时回落内联），实例里只多一个 64 KiB 缓冲。
const AUTO_RESOURCE_THRESHOLD: usize = 1024 * 1024;
const AUTO_RESOURCE_MAX_BYTES: usize = 64 * 1024 * 1024;

pub fn open(method: &str, url: &str, headers: Value, body: Option<&Value>) -> Result<Response> {
    json_params(method, url, headers, body, None)
}

/// 超过 `limit` 即返回 413（不提交请求），让调用方看到“上下文过大”，而不是实例被内存上限终止。
pub fn open_json_resource(method: &str, url: &str, headers: Value, body: &Value, limit: usize) -> Result<Response> {
    json_resource_params(method, url, headers, body, limit, None, false)
}

/// 零分配判断 JSON 序列化后的字节数是否超过上限，避免先生成整段字符串再测量。
pub fn json_exceeds(value: &Value, limit: usize) -> Result<bool> {
    let mut counter = SizeCounter { bytes: 0, limit, overflowed: false };
    match serde_json::to_writer(&mut counter, value) {
        Ok(()) => Ok(false),
        Err(_) if counter.overflowed => Ok(true),
        Err(_) => Err(Error::new("请求正文序列化失败")),
    }
}

/// 需要在 manifest 中声明 http.timeout.v1（依赖 http.v1）。
pub fn open_timeout(method: &str, url: &str, headers: Value, body: Option<&Value>, timeout_ms: u64) -> Result<Response> {
    json_params(method, url, headers, body, Some(timeout_ms))
}

pub fn open_account(method: &str, path: &str, headers: Value, body: Option<&Value>) -> Result<Response> {
    account_params(method, path, headers, body, None)
}

/// 需要在 manifest 中声明 http.timeout.v1（依赖 http.v1）。
pub fn open_account_timeout(method: &str, path: &str, headers: Value, body: Option<&Value>, timeout_ms: u64) -> Result<Response> {
    account_params(method, path, headers, body, Some(timeout_ms))
}

fn account_params(method: &str, path: &str, headers: Value, body: Option<&Value>, timeout_ms: Option<u64>) -> Result<Response> {
    let body = body.map(|value| base64::engine::general_purpose::STANDARD.encode(value.to_string().as_bytes()));
    open_params("http.open_account", json!({ "method": method, "path": path, "headers": headers, "body": body }), timeout_ms)
}

pub fn open_raw(method: &str, url: &str, headers: Value, body: Option<&[u8]>) -> Result<Response> {
    open_raw_params(method, url, headers, body, None)
}

/// 需要在 manifest 中声明 http.timeout.v1（依赖 http.v1）。
pub fn open_raw_timeout(method: &str, url: &str, headers: Value, body: Option<&[u8]>, timeout_ms: u64) -> Result<Response> {
    open_raw_params(method, url, headers, body, Some(timeout_ms))
}

fn json_params(method: &str, url: &str, headers: Value, body: Option<&Value>, timeout_ms: Option<u64>) -> Result<Response> {
    let Some(value) = body else {
        return raw_params(method, url, headers, None, timeout_ms);
    };
    let mut counter = SizeCounter { bytes: 0, limit: AUTO_RESOURCE_THRESHOLD, overflowed: false };
    match serde_json::to_writer(&mut counter, value) {
        Ok(()) => {
            let mut bytes = Vec::with_capacity(counter.bytes);
            serde_json::to_writer(&mut bytes, value).map_err(|_| Error::new("请求正文序列化失败"))?;
            raw_params(method, url, headers, Some(&bytes), timeout_ms)
        }
        Err(_) if counter.overflowed => {
            json_resource_params(method, url, headers, value, AUTO_RESOURCE_MAX_BYTES, timeout_ms, true)
        }
        Err(_) => Err(Error::new("请求正文序列化失败")),
    }
}

fn open_raw_params(method: &str, url: &str, headers: Value, body: Option<&[u8]>, timeout_ms: Option<u64>) -> Result<Response> {
    let Some(bytes) = body.filter(|bytes| bytes.len() > AUTO_RESOURCE_THRESHOLD) else {
        return raw_params(method, url, headers, body, timeout_ms);
    };
    if bytes.len() > AUTO_RESOURCE_MAX_BYTES {
        return Err(Error::coded(
            format!("请求正文超过 {} MiB 的受控上限，请缩小上下文或图片后重试", AUTO_RESOURCE_MAX_BYTES / (1024 * 1024)),
            "plugin_request_too_large",
        ));
    }
    let resource = match crate::images::Resource::create(RESOURCE_MIME) {
        Ok(resource) => resource,
        Err(error) if error.code.as_deref() == Some("forbidden") => {
            return raw_params(method, url, headers, Some(bytes), timeout_ms);
        }
        Err(error) => return Err(error),
    };
    if let Err(error) = resource.append(bytes) {
        let _ = resource.remove();
        return Err(error);
    }
    seal_and_open_resource(method, url, headers, resource, bytes.len(), timeout_ms)
}

fn json_resource_params(
    method: &str, url: &str, headers: Value, body: &Value, limit: usize, timeout_ms: Option<u64>, fallback_inline: bool,
) -> Result<Response> {
    let resource = match crate::images::Resource::create(RESOURCE_MIME) {
        Ok(resource) => resource,
        Err(error) if fallback_inline && error.code.as_deref() == Some("forbidden") => {
            let bytes = serde_json::to_vec(body).map_err(|_| Error::new("请求正文序列化失败"))?;
            return raw_params(method, url, headers, Some(&bytes), timeout_ms);
        }
        Err(mut error) => {
            if !fallback_inline {
                error.message = "请求正文资源创建失败，未提交请求".into();
            }
            return Err(error);
        }
    };
    let (written, total) = {
        let mut writer = BodyWriter::new(&resource, limit);
        let result = serde_json::to_writer(&mut writer, body)
            .map_err(|_| writer.take("请求正文序列化失败"))
            .and_then(|()| writer.finish());
        (result, writer.total)
    };
    if let Err(error) = written {
        let _ = resource.remove();
        return Err(error);
    }
    seal_and_open_resource(method, url, headers, resource, total, timeout_ms)
}

fn seal_and_open_resource(
    method: &str, url: &str, headers: Value, resource: crate::images::Resource, total: usize, timeout_ms: Option<u64>,
) -> Result<Response> {
    let sealed = match resource.seal() {
        Ok(value) => value,
        Err(mut error) => {
            let _ = resource.remove();
            error.message = "请求正文资源封存失败，未提交请求".into();
            return Err(error);
        }
    };
    if sealed["id"].as_str() != Some(resource.id.as_str())
        || sealed["size"].as_u64() != u64::try_from(total).ok()
    {
        let _ = resource.remove();
        return Err(Error::new("请求正文资源封存回执不一致，未提交请求"));
    }
    let result = resource_params(method, url, headers, &resource.id, timeout_ms);
    // 宿主已把字节交给 HTTP 句柄；这里只释放本次调用里的资源槽。
    let _ = resource.remove();
    result
}

/// 宿主只接受这几种资源类型；正文以二进制承载，类型由请求头表达。
const RESOURCE_MIME: &str = "application/octet-stream";

struct SizeCounter {
    bytes: usize,
    limit: usize,
    overflowed: bool,
}

impl Write for SizeCounter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let Some(total) = self.bytes.checked_add(buf.len()).filter(|total| *total <= self.limit) else {
            self.overflowed = true;
            return Err(io::Error::other("size_limit"));
        };
        self.bytes = total;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

fn raw_params(method: &str, url: &str, headers: Value, body: Option<&[u8]>, timeout_ms: Option<u64>) -> Result<Response> {
    open_params("http.open", json!({ "method": method, "url": url, "headers": headers,
        "body": body.map(|body| base64::engine::general_purpose::STANDARD.encode(body)) }), timeout_ms)
}

pub fn open_resource(method: &str, url: &str, headers: Value, resource: &str) -> Result<Response> {
    resource_params(method, url, headers, resource, None)
}

/// 需要在 manifest 中声明 http.timeout.v1（依赖 http.v1）。
pub fn open_resource_timeout(method: &str, url: &str, headers: Value, resource: &str, timeout_ms: u64) -> Result<Response> {
    resource_params(method, url, headers, resource, Some(timeout_ms))
}

fn resource_params(method: &str, url: &str, headers: Value, resource: &str, timeout_ms: Option<u64>) -> Result<Response> {
    open_params("http.open", json!({ "method": method, "url": url, "headers": headers, "body_resource": resource }), timeout_ms)
}

pub fn multipart(method: &str, url: &str, headers: Value, fields: Value, files: Vec<Value>) -> Result<Response> {
    multipart_params(method, url, headers, fields, files, None)
}

/// 需要在 manifest 中声明 http.timeout.v1（依赖 http.v1）。
pub fn multipart_timeout(method: &str, url: &str, headers: Value, fields: Value, files: Vec<Value>, timeout_ms: u64) -> Result<Response> {
    multipart_params(method, url, headers, fields, files, Some(timeout_ms))
}

fn multipart_params(method: &str, url: &str, headers: Value, fields: Value, files: Vec<Value>, timeout_ms: Option<u64>) -> Result<Response> {
    open_params("http.open", json!({ "method": method, "url": url, "headers": headers,
        "multipart": { "fields": fields, "files": files } }), timeout_ms)
}

fn open_params(method: &str, mut params: Value, timeout_ms: Option<u64>) -> Result<Response> {
    if let Some(timeout_ms) = timeout_ms { params["timeout_ms"] = json!(timeout_ms); }
    response(host(method, params)?)
}

fn response(result: Value) -> Result<Response> {
    // 旧宿主没有 headers_raw 时按单值头兼容；已有多值字段必须形状正确，不静默丢 Cookie。
    let headers_raw = match result.get("headers_raw") {
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|_| Error::new("网络回应的多值响应头格式无效"))?,
        None => result["headers"].as_object().into_iter().flatten().filter_map(|(name, value)| {
            value.as_str().map(|value| (name.clone(), value.to_string()))
        }).collect(),
    };
    Ok(Response {
        status: result["status"].as_u64().and_then(|status| u16::try_from(status).ok())
            .ok_or_else(|| Error::new("网络回应缺少状态码"))?,
        headers: result["headers"].clone(),
        headers_raw,
        id: result["id"].as_str().ok_or_else(|| Error::new("网络回应缺少连接标识"))?.to_string(),
        done: false,
    })
}

impl Response {
    pub fn header_values(&self, name: &str) -> Vec<&str> {
        self.headers_raw.iter().filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str()).collect()
    }

    pub fn read(&mut self) -> Result<Option<Vec<u8>>> {
        if self.done { return Ok(None); }
        let result = host("http.read", json!({ "id": self.id }))?;
        if result["done"].as_bool() == Some(true) { self.done = true; return Ok(None); }
        let bytes = base64::engine::general_purpose::STANDARD.decode(result["data"].as_str().unwrap_or(""))
            .map_err(|_| Error::new("网络回应编码无效"))?;
        Ok(Some(bytes))
    }

    pub fn bytes(mut self, limit: usize) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        while let Some(chunk) = self.read()? {
            if output.len().saturating_add(chunk.len()) > limit { return Err(Error::new("上游回应超过大小限制")); }
            output.extend(chunk);
        }
        Ok(output)
    }

    pub fn json(self) -> Result<Value> {
        let status = self.status;
        let bytes = self.bytes(4 * 1024 * 1024)?;
        if !(200..300).contains(&status) { return Err(Error::http(status, format!("上游接口返回 HTTP {status}"))); }
        serde_json::from_slice(&bytes).map_err(|_| Error::new("上游接口未返回有效 JSON"))
    }
}

impl Drop for Response {
    fn drop(&mut self) {
        if !self.done { let _ = host("http.close", json!({ "id": self.id })); }
    }
}

/// 单次写入资源的分片大小；宿主对单个分片的 base64 长度也有上限。
const CHUNK: usize = 64 * 1024;

/// 把 JSON 正文按 64 KiB 分片写进资源：内存里只有一个分片缓冲。
struct BodyWriter<'a> {
    resource: &'a crate::images::Resource,
    buffer: Vec<u8>,
    total: usize,
    limit: usize,
    failure: Option<Error>,
}

impl<'a> BodyWriter<'a> {
    fn new(resource: &'a crate::images::Resource, limit: usize) -> Self {
        Self { resource, buffer: Vec::with_capacity(CHUNK), total: 0, limit, failure: None }
    }

    fn take(&mut self, fallback: &str) -> Error {
        self.failure.take().unwrap_or_else(|| Error::new(fallback))
    }

    fn stopped(&mut self, error: Error) -> io::Error {
        self.failure.get_or_insert(error);
        io::Error::other("请求正文已停止写入")
    }

    fn finish(&mut self) -> Result<()> {
        self.flush().map_err(|_| self.take("请求正文写入失败"))
    }
}

impl Write for BodyWriter<'_> {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        if self.failure.is_some() { return Err(io::Error::other("请求正文已停止写入")); }
        let length = bytes.len();
        let Some(total) = self.total.checked_add(length).filter(|total| *total <= self.limit) else {
            // 只在这里拒绝：继续写下去会先把实例内存撑爆，调用方只会看到笼统的资源错误。
            return Err(self.stopped(Error::coded(
                format!("请求正文超过 {} MiB 的受控上限，请缩小上下文或图片后重试",
                    self.limit / (1024 * 1024)),
                "plugin_request_too_large",
            )));
        };
        while !bytes.is_empty() {
            let take = CHUNK.saturating_sub(self.buffer.len()).min(bytes.len());
            self.buffer.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.buffer.len() == CHUNK { self.flush()?; }
        }
        self.total = total;
        Ok(length)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.failure.is_some() { return Err(io::Error::other("请求正文已停止写入")); }
        if self.buffer.is_empty() { return Ok(()); }
        let result = self.resource.append(&self.buffer);
        if let Err(error) = result { return Err(self.stopped(error)); }
        self.buffer.clear();
        Ok(())
    }
}
