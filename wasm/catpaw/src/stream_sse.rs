//! CatPaw turn 的有界 SSE 分帧；依据原生 catpaw/openai.rs 与 sse_events.rs。
//! 保留 event 字段，不以换行 JSON 猜测 NDJSON；不重连、不使用 id/retry 重放。
//! 完整空行才派发事件，EOF 的残帧只供错误检查，绝不能确认 DONE。

use agent2api_wasm_sdk::Result;

use crate::errors;

pub(super) const MAX_EVENT: usize = 1024 * 1024;
const MAX_WIRE: usize = 64 * 1024 * 1024;
const MAX_EVENTS: usize = 16_384;

pub(super) struct Event {
    pub(super) name: Option<String>,
    pub(super) data: String,
    pub(super) complete: bool,
}

#[derive(Default)]
pub(super) struct Reader {
    line: Vec<u8>,
    data: String,
    name: Option<String>,
    has_data: bool,
    first_line: bool,
    skip_lf: bool,
    event_bytes: usize,
    wire_bytes: usize,
    events: usize,
}

impl Reader {
    pub(super) fn new() -> Self {
        Self {
            first_line: true,
            ..Self::default()
        }
    }

    /// 回调同步消费一个事件，不攒整条流或无界事件队列。
    pub(super) fn push(
        &mut self,
        bytes: &[u8],
        mut consume: impl FnMut(Event) -> Result<()>,
    ) -> Result<()> {
        self.wire_bytes = self
            .wire_bytes
            .checked_add(bytes.len())
            .filter(|size| *size <= MAX_WIRE)
            .ok_or_else(|| errors::protocol("CatPaw 上游流超过 64 MiB 读取预算"))?;
        for &byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            self.event_bytes += 1;
            if self.event_bytes > MAX_EVENT {
                return Err(errors::protocol(
                    "CatPaw SSE 事件超过 1 MiB，未跳过异常事件",
                ));
            }
            if matches!(byte, b'\r' | b'\n') {
                self.consume_line(&mut consume)?;
                self.skip_lf = byte == b'\r';
            } else {
                self.line.push(byte);
            }
        }
        Ok(())
    }

    fn consume_line(&mut self, consume: &mut impl FnMut(Event) -> Result<()>) -> Result<()> {
        let raw = std::mem::take(&mut self.line);
        let bytes = if self.first_line {
            raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&raw)
        } else {
            &raw
        };
        self.first_line = false;
        let line = std::str::from_utf8(bytes)
            .map_err(|_| errors::protocol("CatPaw SSE 包含无效 UTF-8，未替换或丢弃字符"))?;
        if line.is_empty() {
            self.dispatch(true, consume)?;
            self.event_bytes = 0;
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "event" => {
                if value.len() > 128 || value.chars().any(char::is_control) {
                    return Err(errors::protocol("CatPaw SSE event 名称过长或包含控制字符"));
                }
                if self
                    .name
                    .as_deref()
                    .is_some_and(|previous| previous != value)
                {
                    return Err(errors::protocol("CatPaw 同一 SSE 事件出现冲突名称"));
                }
                self.name = Some(value.to_string());
            }
            // SSE 传输元数据不进入重连或任务所有权；仅按事件预算消费。
            "id" | "retry" => {}
            _ => {
                return Err(errors::protocol(
                    "CatPaw SSE 含未知字段或非 SSE 数据，未猜测另一种流协议",
                ))
            }
        }
        Ok(())
    }

    fn dispatch(
        &mut self,
        complete: bool,
        consume: &mut impl FnMut(Event) -> Result<()>,
    ) -> Result<()> {
        if !self.has_data && self.name.is_none() {
            return Ok(());
        }
        self.events += 1;
        if self.events > MAX_EVENTS {
            return Err(errors::protocol("CatPaw SSE 事件数量超过 16384 项"));
        }
        let event = Event {
            name: self.name.take(),
            data: std::mem::take(&mut self.data),
            complete,
        };
        self.has_data = false;
        consume(event)
    }

    pub(super) fn finish(&mut self, mut consume: impl FnMut(Event) -> Result<()>) -> Result<()> {
        if !self.line.is_empty() {
            self.consume_line(&mut consume)?;
        }
        self.dispatch(false, &mut consume)?;
        if self.event_bytes != 0 {
            return Err(errors::protocol("CatPaw EOF 留下未结束的 SSE 元数据或注释"));
        }
        Ok(())
    }
}
