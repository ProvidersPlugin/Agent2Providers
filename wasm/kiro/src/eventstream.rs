//! AWS eventstream 二进制帧，不把字节流当 SSE 字符串喂转换器。
//! 基于原生 kiro/eventstream.rs，按 Smithy Amazon Event Stream 规范补上双 CRC32：
//! prelude=[0..8)，message=[0..total-4)。所有整数大端，header重复/越界/未知类型拒绝。
//! 单帧16MiB、headers128KiB、单次读64KiB；残帧EOF永远报错，不假补成功终态。

use crate::errors;
use agent2api_wasm_sdk::{http, Error, Result};
use std::collections::HashSet;

const MAX_FRAME: usize = 16 * 1024 * 1024;
const MAX_HEADERS: usize = 128 * 1024;
const MAX_CHUNK: usize = 64 * 1024;

pub struct Frame {
    pub event_type: String,
    pub message_type: String,
    pub exception_type: String,
    pub error_code: String,
    pub payload: Vec<u8>,
}
impl Frame {
    pub fn error(&self) -> bool {
        matches!(self.message_type.as_str(), "exception" | "error")
            || !self.exception_type.is_empty()
            || !self.error_code.is_empty()
            || matches!(
                self.event_type.as_str(),
                "error"
                    | "invalidStateEvent"
                    | "throttlingError"
                    | "validationError"
                    | "serviceUnavailableError"
                    | "internalServerException"
            )
    }
    pub fn error_kind(&self) -> &str {
        if !self.exception_type.is_empty() {
            &self.exception_type
        } else if !self.error_code.is_empty() {
            &self.error_code
        } else {
            &self.event_type
        }
    }
}
fn bad(message: &str) -> Error {
    Error::http(502, message).with_code("kiro_eventstream_invalid")
}
fn u32_at(bytes: &[u8], at: usize) -> Result<u32> {
    let value = bytes
        .get(at..at + 4)
        .ok_or_else(|| bad("Kiro 事件帧整数被截断"))?;
    Ok(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
}
fn take<'a>(bytes: &'a [u8], index: &mut usize, length: usize) -> Result<&'a [u8]> {
    let end = index
        .checked_add(length)
        .ok_or_else(|| bad("Kiro 事件头长度溢出"))?;
    let value = bytes
        .get(*index..end)
        .ok_or_else(|| bad("Kiro 事件头被截断"))?;
    *index = end;
    Ok(value)
}
fn headers(bytes: &[u8]) -> Result<Frame> {
    let mut out = Frame {
        event_type: String::new(),
        message_type: String::new(),
        exception_type: String::new(),
        error_code: String::new(),
        payload: Vec::new(),
    };
    let mut names = HashSet::new();
    let mut index = 0;
    while index < bytes.len() {
        let length = take(bytes, &mut index, 1)?[0] as usize;
        if length == 0 || names.len() >= 256 {
            return Err(bad("Kiro 事件头名称或数量无效"));
        }
        let name = std::str::from_utf8(take(bytes, &mut index, length)?)
            .map_err(|_| bad("Kiro 事件头名称不是有效UTF-8"))?;
        if name.chars().any(char::is_control) || !names.insert(name.to_string()) {
            return Err(bad("Kiro 事件头重复或包含控制字符"));
        }
        let kind = take(bytes, &mut index, 1)?[0];
        let required = matches!(
            name,
            ":event-type" | ":message-type" | ":exception-type" | ":error-code"
        );
        if required && kind != 7 {
            return Err(bad("Kiro 事件类型头必须为字符串"));
        }
        let length = match kind {
            0 | 1 => 0,
            2 => 1,
            3 => 2,
            4 => 4,
            5 | 8 => 8,
            9 => 16,
            6 | 7 => {
                let length = take(bytes, &mut index, 2)?;
                u16::from_be_bytes([length[0], length[1]]) as usize
            }
            _ => return Err(bad("Kiro 事件头使用未知编码类型")),
        };
        let raw = take(bytes, &mut index, length)?;
        if kind == 7 {
            let text = std::str::from_utf8(raw).map_err(|_| bad("Kiro 事件头值不是有效UTF-8"))?;
            if required
                && (text.is_empty() || text.len() > 1024 || text.chars().any(char::is_control))
            {
                return Err(bad("Kiro 事件类型值无效"));
            }
            match name {
                ":event-type" => out.event_type = text.into(),
                ":message-type" => out.message_type = text.into(),
                ":exception-type" => out.exception_type = text.into(),
                ":error-code" => out.error_code = text.into(),
                _ => {}
            }
        }
    }
    if !matches!(out.message_type.as_str(), "event" | "exception" | "error")
        || out.message_type == "event" && out.event_type.is_empty()
    {
        return Err(bad("Kiro 事件帧缺少有效类型"));
    }
    Ok(out)
}
pub struct Reader {
    response: http::Response,
    buffer: Vec<u8>,
    offset: usize,
    ended: bool,
    frames: usize,
}
impl Reader {
    pub fn new(response: http::Response) -> Self {
        Self {
            response,
            buffer: Vec::new(),
            offset: 0,
            ended: false,
            frames: 0,
        }
    }
    fn frame(&mut self) -> Result<Option<Frame>> {
        let bytes = &self.buffer[self.offset..];
        if bytes.len() < 12 {
            return Ok(None);
        }
        let total = u32_at(bytes, 0)? as usize;
        let header_len = u32_at(bytes, 4)? as usize;
        if !(16..=MAX_FRAME).contains(&total) || header_len > total - 16 || header_len > MAX_HEADERS
        {
            return Err(bad("Kiro 事件帧长度无效或超出上限"));
        }
        if crc32fast::hash(&bytes[..8]) != u32_at(bytes, 8)? {
            return Err(bad("Kiro 事件帧前导校验失败"));
        }
        if bytes.len() < total {
            return Ok(None);
        }
        if crc32fast::hash(&bytes[..total - 4]) != u32_at(bytes, total - 4)? {
            return Err(bad("Kiro 事件帧完整校验失败"));
        }
        let mut frame = headers(&bytes[12..12 + header_len])?;
        frame.payload = bytes[12 + header_len..total - 4].to_vec();
        self.offset += total;
        self.frames += 1;
        if self.frames > 100_000 {
            return Err(bad("Kiro 事件数量超出单次请求上限"));
        }
        Ok(Some(frame))
    }
    pub fn next(&mut self) -> Result<Option<Frame>> {
        loop {
            if let Some(frame) = self.frame()? {
                return Ok(Some(frame));
            }
            if self.ended {
                return if self.buffer.len() == self.offset {
                    Ok(None)
                } else {
                    Err(bad("Kiro 二进制事件流提前结束，最后一帧不完整"))
                };
            }
            if self.offset > 0 {
                self.buffer.drain(..self.offset);
                self.offset = 0;
            }
            match self.response.read().map_err(errors::transport)? {
                Some(bytes) => {
                    if bytes.len() > MAX_CHUNK
                        || self.buffer.len().saturating_add(bytes.len()) > MAX_FRAME + MAX_CHUNK
                    {
                        return Err(bad("Kiro 事件缓冲超过受控上限"));
                    }
                    self.buffer.extend(bytes);
                }
                None => self.ended = true,
            }
        }
    }
}
