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

pub fn open(method: &str, url: &str, headers: Value, body: Option<&Value>) -> Result<Response> {
    open_raw(method, url, headers, body.map(|body| body.to_string()).as_deref().map(str::as_bytes))
}

/// 大正文的发送通道：先分片写入当前调用资源，再由宿主直接取原字节发出。
///
/// 普通 `open` 会把整份正文变成 String，再 base64 成文本塞进外层信封 ——
/// 发送期间正文至少存在三份（原 Value、字符串、base64 文本与信封字节）。
/// 对话这类 MB 级正文用这条路径，实例里只多一个 64 KiB 的分片缓冲。
///
/// `limit` 是允许的最大正文字节数；超过即返回 413（不提交请求），
/// 让调用方看到“上下文过大”，而不是实例被内存上限终止。
pub fn open_json_resource(method: &str, url: &str, headers: Value, body: &Value, limit: usize) -> Result<Response> {
    let resource = crate::images::Resource::create(RESOURCE_MIME).map_err(|mut error| {
        error.message = "请求正文资源创建失败，未提交请求".into();
        error
    })?;
    let written = {
        let mut writer = BodyWriter::new(&resource, limit);
        let result = serde_json::to_writer(&mut writer, body)
            .map_err(|_| writer.take("请求正文序列化失败"))
            .and_then(|()| writer.finish());
        (result, writer.total)
    };
    let (written, total) = written;
    if let Err(error) = written {
        let _ = resource.remove();
        return Err(error);
    }
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
    let result = open_resource(method, url, headers, &resource.id);
    // 宿主已把字节交给 HTTP 句柄；这里只释放本次调用里的资源槽。
    let _ = resource.remove();
    result
}

/// 宿主只接受这几种资源类型；正文以二进制承载，类型由请求头表达。
const RESOURCE_MIME: &str = "application/octet-stream";

/// 需要在 manifest 中声明 http.timeout.v1（依赖 http.v1）。
pub fn open_timeout(method: &str, url: &str, headers: Value, body: Option<&Value>, timeout_ms: u64) -> Result<Response> {
    open_raw_timeout(method, url, headers, body.map(|body| body.to_string()).as_deref().map(str::as_bytes), timeout_ms)
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
    raw_params(method, url, headers, body, None)
}

/// 需要在 manifest 中声明 http.timeout.v1（依赖 http.v1）。
pub fn open_raw_timeout(method: &str, url: &str, headers: Value, body: Option<&[u8]>, timeout_ms: u64) -> Result<Response> {
    raw_params(method, url, headers, body, Some(timeout_ms))
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
