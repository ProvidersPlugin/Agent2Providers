//! AWS event stream 帧解析（Kiro 的响应格式）。
//!
//! ── 帧结构 ──────────────────────────────────────────────────
//! ```text
//!   [0..4)   total length（含所有字段）
//!   [4..8)   headers length
//!   [8..12)  prelude CRC（不校验）
//!   [12..12+headers)
//!   [..total-4) payload
//!   [total-4..total) message CRC（不校验）
//! ```
//!
//! 每个 header 是 `名称长度(1) 名称 类型(1) 值`：
//!   0=true / 1=false（无字节）、2=byte、3=short、4=int、5=long、8=timestamp、
//!   9=uuid、6=bytes（2 字节长度）、7=string（2 字节长度）。
//!
//! 我们只关心类型 7 的字符串：`:message-type`（event / exception / error）、
//! `:event-type`、`:exception-type`、`:error-code`。
//!
//! ── 为什么要自己解析 ────────────────────────────────────────
//! 网关的 `UpstreamKind` 是封闭枚举（Chat / Anthropic / Ndjson / Gemini /
//! 混合快照），没有 AWS event stream。声明 `stateful` 由插件自己读上游、
//! 自己推 OpenAI chat SSE 是唯一不扩协议的路。

use bytes::Bytes;
use futures::{stream::BoxStream, StreamExt};

/// 一个解析出来的帧。
#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub event_type: String,
    pub message_type: String,
    pub exception_type: String,
    pub error_code: String,
    pub payload: Vec<u8>,
}

impl Frame {
    /// 这一帧是不是错误（exception / error / 带错误码的事件）。
    pub fn is_error(&self) -> bool {
        self.message_type == "exception"
            || self.message_type == "error"
            || matches!(
                self.event_type.as_str(),
                "error"
                    | "throttlingError"
                    | "validationError"
                    | "serviceUnavailableError"
                    | "internalServerException"
            )
    }

    /// 错误的类型名（异常类型优先，其次事件名）。
    pub fn error_kind(&self) -> String {
        if !self.exception_type.is_empty() {
            self.exception_type.clone()
        } else if !self.error_code.is_empty() {
            self.error_code.clone()
        } else {
            self.event_type.clone()
        }
    }
}

/// 增量帧读取器。
pub struct FrameReader {
    stream: BoxStream<'static, Result<Bytes, reqwest::Error>>,
    buffer: Vec<u8>,
    ended: bool,
}

impl FrameReader {
    pub fn new(response: reqwest::Response) -> Self {
        Self {
            stream: Box::pin(response.bytes_stream()),
            buffer: Vec::new(),
            ended: false,
        }
    }

    /// 读下一帧（流结束返回 `Ok(None)`）。
    pub async fn next(&mut self) -> Result<Option<Frame>, String> {
        loop {
            if let Some(frame) = self.take_frame()? {
                return Ok(Some(frame));
            }
            if self.ended {
                if self.buffer.is_empty() {
                    return Ok(None);
                }
                return Err("Kiro 的事件流提前结束了".to_string());
            }
            match self.stream.next().await {
                Some(Ok(chunk)) => self.buffer.extend_from_slice(&chunk),
                Some(Err(error)) => {
                    return Err(format!(
                        "读取 Kiro 的事件流失败：{}",
                        crate::http::describe(&error)
                    ))
                }
                None => self.ended = true,
            }
        }
    }

    /// 缓冲区里已经有一整帧就取出来。
    fn take_frame(&mut self) -> Result<Option<Frame>, String> {
        if self.buffer.len() < 12 {
            return Ok(None);
        }
        let total = u32::from_be_bytes([
            self.buffer[0],
            self.buffer[1],
            self.buffer[2],
            self.buffer[3],
        ]) as usize;
        let header_length = u32::from_be_bytes([
            self.buffer[4],
            self.buffer[5],
            self.buffer[6],
            self.buffer[7],
        ]) as usize;
        if total < 16 || total > (16 << 20) || header_length > total.saturating_sub(16) {
            return Err("Kiro 的事件流帧格式不对".to_string());
        }
        if self.buffer.len() < total {
            return Ok(None);
        }
        let frame = &self.buffer[..total];
        let headers = parse_headers(&frame[12..12 + header_length]);
        let payload = frame[12 + header_length..total - 4].to_vec();
        self.buffer.drain(..total);
        Ok(Some(Frame {
            payload,
            event_type: headers.0,
            message_type: headers.1,
            exception_type: headers.2,
            error_code: headers.3,
        }))
    }
}

/// 解析 header 段，返回（event-type, message-type, exception-type, error-code）。
fn parse_headers(bytes: &[u8]) -> (String, String, String, String) {
    let mut event_type = String::new();
    let mut message_type = String::new();
    let mut exception_type = String::new();
    let mut error_code = String::new();
    let mut index = 0;
    while index < bytes.len() {
        let name_length = bytes[index] as usize;
        index += 1;
        if index + name_length + 1 > bytes.len() {
            break;
        }
        let name = String::from_utf8_lossy(&bytes[index..index + name_length]).to_string();
        index += name_length;
        let kind = bytes[index];
        index += 1;
        match kind {
            0 | 1 => {
                if name == ":message-type" {
                    message_type = if kind == 0 { "event" } else { "error" }.to_string();
                }
            }
            2 | 3 | 4 | 5 | 8 | 9 => {
                let size = match kind {
                    2 => 1,
                    3 => 2,
                    4 => 4,
                    5 | 8 => 8,
                    _ => 16,
                };
                index += size;
            }
            6 | 7 => {
                if index + 2 > bytes.len() {
                    break;
                }
                let length = u16::from_be_bytes([bytes[index], bytes[index + 1]]) as usize;
                index += 2;
                if index + length > bytes.len() {
                    break;
                }
                if kind == 7 {
                    let value = String::from_utf8_lossy(&bytes[index..index + length]).to_string();
                    match name.as_str() {
                        ":event-type" => event_type = value,
                        ":message-type" => message_type = value,
                        ":exception-type" => exception_type = value,
                        ":error-code" => error_code = value,
                        _ => {}
                    }
                }
                index += length;
            }
            _ => break,
        }
    }
    (event_type, message_type, exception_type, error_code)
}
