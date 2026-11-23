//! JSON 控制面与资源正文只经 SDK 出网；账号代理失效不静默改成直连。

use super::{errors, util};
use agent2api_wasm_sdk::{http, images::Resource, json, Error, Result, Value};

pub struct Reply {
    pub status: u16,
    pub body: Value,
}
impl Reply {
    pub fn accepted(self) -> Result<Value> {
        if !(200..300).contains(&self.status) || errors::failed(&self.body) {
            return Err(errors::upstream(self.status, &self.body));
        }
        if !self.body.is_object() {
            return Err(Error::http(502, "Antigravity 上游没有返回有效 JSON 对象"));
        }
        Ok(self.body)
    }
}

pub fn read(response: http::Response, limit: usize) -> Result<Reply> {
    let status = response.status;
    let values = response.header_values("retry-after");
    let retry_at = if values.len() == 1 {
        let raw = values[0].trim();
        raw.parse::<u64>()
            .ok()
            .filter(|s| *s <= 7 * 86400)
            .and_then(|s| {
                util::now()
                    .ok()
                    .map(|now| now.saturating_add((s * 1000) as i64))
            })
            .or_else(|| {
                chrono::DateTime::parse_from_rfc2822(raw)
                    .ok()
                    .map(|at| at.timestamp_millis())
            })
    } else {
        None
    };
    let bytes = response.bytes(limit).map_err(errors::transport)?;
    let mut body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if status == 429 || errors::failed(&body) {
        if let Some(at) = retry_at.filter(|at| *at > 0) {
            if !body.is_object() {
                body = json!({});
            }
            body["resetAt"] = json!(at);
        }
    }
    Ok(Reply { status, body })
}

// 原生目录/额度及项目发现均为 20 秒；聊天与图片只走下面不设短总期限的 stream。
pub fn post(url: &str, headers: Value, body: &Value) -> Result<Reply> {
    read(
        http::open_timeout("POST", url, headers, Some(body), 20_000).map_err(errors::transport)?,
        4 * 1024 * 1024,
    )
}

// 仅用于 OAuth userinfo v2/v1，沿用原生每次 20 秒。
pub fn get(url: &str, headers: Value) -> Result<Reply> {
    read(
        http::open_timeout("GET", url, headers, None, 20_000).map_err(errors::transport)?,
        1024 * 1024,
    )
}

// 仅用于 OAuth 换码/续期；30 秒到期不在这里重发一次性材料。
pub fn form(url: &str, fields: &[(&str, &str)], user_agent: &str) -> Result<Reply> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields.iter().copied())
        .finish();
    let headers = json!({ "Content-Type": "application/x-www-form-urlencoded", "Accept": "application/json", "User-Agent": user_agent });
    read(
        http::open_raw_timeout("POST", url, headers, Some(body.as_bytes()), 30_000)
            .map_err(errors::transport)?,
        1024 * 1024,
    )
}

/// 大信封分片序列化到当前调用资源，不分配另一份完整信封或Base64 ABI消息。
/// 仍受入站 ABI 16MiB / 本模块 48MiB / 主机资源总量 96MiB 约束，超限明确失败。
pub fn stream(url: &str, headers: Value, body: &Value) -> Result<http::Response> {
    let resource = Resource::create("application/octet-stream")?;
    let mut writer = ResourceWriter {
        resource: &resource,
        pending: Vec::with_capacity(64 * 1024),
        total: 0,
        failure: None,
    };
    if serde_json::to_writer(&mut writer, body).is_err() {
        return Err(writer
            .failure
            .take()
            .unwrap_or_else(|| Error::http(400, "Antigravity 请求不能编码")));
    }
    if writer.flush_pending().is_err() {
        return Err(writer
            .failure
            .take()
            .unwrap_or_else(|| Error::http(502, "Antigravity 请求资源未完整写入")));
    }
    drop(writer);
    resource.seal()?;
    let response =
        http::open_resource("POST", url, headers, &resource.id).map_err(errors::transport);
    let _ = resource.remove();
    response
}

struct ResourceWriter<'a> {
    resource: &'a Resource,
    pending: Vec<u8>,
    total: usize,
    failure: Option<Error>,
}
impl ResourceWriter<'_> {
    fn failed(&mut self, error: Error) -> std::io::Error {
        self.failure = Some(error);
        std::io::Error::other("Antigravity request resource failed")
    }
    fn flush_pending(&mut self) -> std::io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        if let Err(error) = self.resource.append(&self.pending) {
            return Err(self.failed(error));
        }
        self.pending.clear();
        Ok(())
    }
}
impl std::io::Write for ResourceWriter<'_> {
    fn write(&mut self, mut bytes: &[u8]) -> std::io::Result<usize> {
        let length = bytes.len();
        if self.total.saturating_add(length) > 48 * 1024 * 1024 {
            return Err(self.failed(
                Error::http(413, "Antigravity 请求信封超过资源上限，未丢弃图片")
                    .with_code("plugin_resource_limit"),
            ));
        }
        self.total += length;
        while !bytes.is_empty() {
            let take = bytes.len().min(64 * 1024 - self.pending.len());
            self.pending.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.pending.len() == 64 * 1024 {
                self.flush_pending()?;
            }
        }
        Ok(length)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.flush_pending()
    }
}
