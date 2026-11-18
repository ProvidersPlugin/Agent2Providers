//! 跨UTF-8/CRLF边界的本家SSE读取；EOF冲刷尾事件但不生成成功终态。
//! Images独立上限允许大inlineData，不把它送进宿主1MiB的聊天转换器。

use super::errors;
use agent2api_wasm_sdk::{http, Error, Result};
use std::collections::VecDeque;

pub struct Event {
    pub name: String,
    pub data: String,
}
pub struct Reader {
    response: http::Response,
    line: Vec<u8>,
    data: Vec<u8>,
    name: String,
    skip_lf: bool,
    ready: VecDeque<Event>,
    eof: bool,
    maximum: usize,
    total: usize,
}
impl Reader {
    pub fn new(response: http::Response, maximum: usize) -> Self {
        Self {
            response,
            line: Vec::new(),
            data: Vec::new(),
            name: String::new(),
            skip_lf: false,
            ready: VecDeque::new(),
            eof: false,
            maximum,
            total: 0,
        }
    }
    fn emit(&mut self) -> Result<()> {
        if !self.data.is_empty() {
            if self.data.last() == Some(&b'\n') {
                self.data.pop();
            }
            self.ready.push_back(Event {
                name: std::mem::take(&mut self.name),
                data: String::from_utf8(std::mem::take(&mut self.data))
                    .map_err(|_| Error::http(502, "Antigravity 流事件不是有效UTF-8"))?,
            });
        } else {
            self.name.clear();
        }
        Ok(())
    }
    fn line(&mut self) -> Result<()> {
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            return self.emit();
        }
        if let Some(data) = line.strip_prefix(b"data:") {
            let data = data.strip_prefix(b" ").unwrap_or(data);
            if self.data.len().saturating_add(data.len()).saturating_add(1) > self.maximum {
                return Err(self.large());
            }
            if self.data.is_empty() {
                self.data = line;
                let prefix = if self.data.get(5) == Some(&b' ') {
                    6
                } else {
                    5
                };
                self.data.drain(..prefix);
            } else {
                self.data.extend_from_slice(data);
            }
            self.data.push(b'\n');
        } else if let Some(name) = line.strip_prefix(b"event:") {
            self.name = String::from_utf8(name.to_vec())
                .map_err(|_| Error::http(502, "Antigravity 流事件名称无效"))?
                .trim()
                .into();
        }
        Ok(())
    }
    fn large(&self) -> Error {
        Error::http(413, "Antigravity 流事件超过当前资源上限，未截断或丢弃图片")
            .with_code("plugin_resource_limit")
    }
    fn push(&mut self, bytes: &[u8]) -> Result<()> {
        self.total = self.total.saturating_add(bytes.len());
        if self.total > 128 * 1024 * 1024 {
            return Err(self.large());
        }
        for &byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if matches!(byte, b'\r' | b'\n') {
                self.line()?;
                self.skip_lf = byte == b'\r';
            } else {
                if self.line.len().saturating_add(self.data.len()) >= self.maximum {
                    return Err(self.large());
                }
                self.line.push(byte);
            }
        }
        Ok(())
    }
    pub fn next(&mut self) -> Result<Option<Event>> {
        loop {
            if let Some(event) = self.ready.pop_front() {
                return Ok(Some(event));
            }
            if self.eof {
                return Ok(None);
            }
            match self.response.read().map_err(errors::transport)? {
                Some(bytes) => self.push(&bytes)?,
                None => {
                    self.eof = true;
                    if !self.line.is_empty() {
                        self.line()?;
                    }
                    self.emit()?;
                }
            }
        }
    }
}
