//! 固定国内来源的受控HTTP，错误不回显登录state、token或完整响应。
use super::{endpoints, errors};
use agent2api_wasm_sdk::{http, Error, Result, Value};

pub struct Reply {
    pub status: u16,
    pub value: Value,
}
impl Reply {
    pub fn ok(&self) -> bool {
        !errors::failed(self.status, &self.value)
    }
    pub fn data(&self) -> &Value {
        self.value.get("data").unwrap_or(&Value::Null)
    }
    pub fn code(&self) -> Option<i64> {
        errors::code(&self.value)
    }
    pub fn message(&self) -> &str {
        errors::message(&self.value)
    }
    pub fn checked(self, secrets: &[&str]) -> Result<Value> {
        if !self.ok() {
            return Err(errors::upstream(self.status, &self.value, secrets));
        }
        Ok(self.value.get("data").cloned().unwrap_or(Value::Null))
    }
}
pub fn open(
    method: &str,
    origin: &str,
    path: &str,
    mut headers: Value,
    body: Option<&Value>,
) -> Result<http::Response> {
    if body.is_some() {
        if let Some(fields) = headers.as_object_mut() {
            fields.retain(|name, _| !name.eq_ignore_ascii_case("content-type"));
            fields.insert(
                "Content-Type".into(),
                Value::String("application/json".into()),
            );
        }
    }
    http::open(method, &endpoints::url(origin, path)?, headers, body).map_err(|mut e| {
        e.message = "WorkBuddy网络请求未完成".into();
        e
    })
}
/// 对话正文的最大字节数：与图片编辑入口的内联数据上限一致。
const MAX_CHAT_BODY: usize = 14 * 1024 * 1024;
/// 中小正文直接内联发出以兼容旧版宿主固定入口，大正文走分片资源通道避免撑爆实例内存。
const INLINE_CHAT_BODY: usize = 4 * 1024 * 1024;

struct SizeCounter {
    bytes: usize,
    limit: usize,
    overflowed: bool,
}

impl std::io::Write for SizeCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let Some(total) = self.bytes.checked_add(buf.len()).filter(|total| *total <= self.limit) else {
            self.overflowed = true;
            return Err(std::io::Error::other("inline_limit"));
        };
        self.bytes = total;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn inline_json_bytes(body: &Value) -> Result<Option<Vec<u8>>> {
    let mut counter = SizeCounter {
        bytes: 0,
        limit: INLINE_CHAT_BODY,
        overflowed: false,
    };
    match serde_json::to_writer(&mut counter, body) {
        Ok(()) => {
            let mut bytes = Vec::with_capacity(counter.bytes);
            serde_json::to_writer(&mut bytes, body)
                .map_err(|_| Error::http(400, "WorkBuddy请求正文序列化失败"))?;
            Ok(Some(bytes))
        }
        Err(_) if counter.overflowed => Ok(None),
        Err(_) => Err(Error::http(400, "WorkBuddy请求正文序列化失败")),
    }
}

fn chat_transport_error(mut error: Error) -> Error {
    // 宿主的上限、参数、重放拦截与超时等结构化错误本身不回显凭据，保留原原因以便排查；
    // 只有未分类的底层传输失败才换成笼统文案。
    let preserve = matches!(
        error.code.as_deref(),
        Some(
            "plugin_request_too_large"
                | "bad_params"
                | "forbidden"
                | "request_replay_blocked"
                | "plugin_resource_limit"
                | "timeout"
                | "aborted"
        )
    );
    if !preserve {
        error.message = "WorkBuddy网络请求未完成".into();
    }
    error
}

/// 中小正文走内联发送，大正文分片写入调用资源并由宿主取原字节发送。
pub fn open_chat(
    origin: &str,
    path: &str,
    mut headers: Value,
    body: &Value,
) -> Result<http::Response> {
    headers["Content-Type"] = Value::String("application/json".into());
    let url = endpoints::url(origin, path)?;
    let response = match inline_json_bytes(body)? {
        Some(bytes) => http::open_raw("POST", &url, headers, Some(&bytes)),
        None => http::open_json_resource("POST", &url, headers, body, MAX_CHAT_BODY),
    };
    response.map_err(chat_transport_error)
}
pub fn open_edit_resource(
    origin: &str,
    mut headers: Value,
    resource: &agent2api_wasm_sdk::images::Resource,
) -> Result<http::Response> {
    headers["Content-Type"] = Value::String("application/json".into());
    http::open_resource(
        "POST",
        &endpoints::url(origin, "/v2/images/edits")?,
        headers,
        &resource.id,
    )
    .map_err(|mut error| {
        error.message = "WorkBuddy网络请求未完成".into();
        error
    })
}

pub fn read(response: http::Response, max: usize) -> Result<Reply> {
    let status = response.status;
    let bytes = response.bytes(max).map_err(|mut e| {
        e.message = "WorkBuddy响应读取失败或过大".into();
        e
    })?;
    let value = serde_json::from_slice(&bytes).map_err(|_| {
        Error::http(
            if (400..600).contains(&status) {
                status
            } else {
                502
            },
            "WorkBuddy未返回合法JSON",
        )
    })?;
    Ok(Reply { status, value })
}
pub fn raw(
    method: &str,
    origin: &str,
    path: &str,
    headers: Value,
    body: Option<&Value>,
) -> Result<Reply> {
    read(open(method, origin, path, headers, body)?, 4 * 1024 * 1024)
}
pub fn raw_timeout(
    method: &str,
    origin: &str,
    path: &str,
    mut headers: Value,
    body: Option<&Value>,
    timeout_ms: u64,
) -> Result<Reply> {
    if body.is_some() {
        if let Some(fields) = headers.as_object_mut() {
            fields.retain(|name, _| !name.eq_ignore_ascii_case("content-type"));
            fields.insert(
                "Content-Type".into(),
                Value::String("application/json".into()),
            );
        }
    }
    let response = http::open_timeout(
        method,
        &endpoints::url(origin, path)?,
        headers,
        body,
        timeout_ms,
    )
    .map_err(|mut error| {
        error.message = "WorkBuddy网络请求未完成".into();
        error
    })?;
    read(response, 4 * 1024 * 1024)
}
pub fn request(
    method: &str,
    origin: &str,
    path: &str,
    headers: Value,
    body: Option<&Value>,
    secrets: &[&str],
) -> Result<Value> {
    raw(method, origin, path, headers, body)?.checked(secrets)
}
