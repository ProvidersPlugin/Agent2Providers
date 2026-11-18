//! Accio SSE 字节分帧；同一网络分片中的后续事件不会丢失，UTF-8 只在完整帧解码。

use crate::errors;
use agent2api_wasm_sdk::{http, Error, Result};

const MAX_EVENT: usize = 1024 * 1024;

pub struct Event {
    pub name: String,
    pub data: String,
}

pub struct Reader {
    response: http::Response,
    bytes: Vec<u8>,
    eof: bool,
    total: usize,
}

impl Reader {
    pub fn new(response: http::Response) -> Self {
        Self {
            response,
            bytes: Vec::new(),
            eof: false,
            total: 0,
        }
    }
    pub fn next(&mut self) -> Result<Option<Event>> {
        loop {
            if let Some(frame) = take(&mut self.bytes) {
                if let Some(event) = parse(&frame)? {
                    return Ok(Some(event));
                }
                continue;
            }
            if self.bytes.len() > MAX_EVENT {
                return Err(Error::http(502, "Accio 上游单个事件超过 1 MiB"));
            }
            if self.eof {
                if self.bytes.is_empty() {
                    return Ok(None);
                }
                return parse(&std::mem::take(&mut self.bytes));
            }
            match self.response.read().map_err(errors::transport)? {
                Some(bytes) => {
                    self.total = self.total.saturating_add(bytes.len());
                    if self.total > 64 * 1024 * 1024 {
                        return Err(Error::http(502, "Accio 上游流超过 64 MiB 限制"));
                    }
                    self.bytes.extend_from_slice(&bytes);
                }
                None => self.eof = true,
            }
        }
    }
}

fn take(bytes: &mut Vec<u8>) -> Option<Vec<u8>> {
    let mut start = 0;
    for at in 0..bytes.len() {
        if bytes[at] != b'\n' {
            continue;
        }
        let end = if at > start && bytes[at - 1] == b'\r' {
            at - 1
        } else {
            at
        };
        if end == start {
            let frame = bytes[..start].to_vec();
            bytes.drain(..=at);
            return Some(frame);
        }
        start = at + 1;
    }
    None
}

fn parse(bytes: &[u8]) -> Result<Option<Event>> {
    if bytes.len() > MAX_EVENT {
        return Err(Error::http(502, "Accio 上游单个事件超过 1 MiB"));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Error::http(502, "Accio 上游返回了不完整的 UTF-8"))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut name = String::new();
    let mut data = Vec::new();
    for line in text.split('\n').map(|line| line.trim_end_matches('\r')) {
        if let Some(value) = line.strip_prefix("event:") {
            name = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    Ok((!name.is_empty() || !data.is_empty()).then(|| Event {
        name,
        data: data.join("\n"),
    }))
}
