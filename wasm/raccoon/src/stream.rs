use agent2api_wasm_sdk::{self as sdk, sse::SseDecoder, Error, Result};

pub(super) struct Stream {
    decoder: SseDecoder,
    bytes: usize,
    done: bool,
    started: bool,
}

impl Stream {
    pub(super) fn new() -> Self {
        Self { decoder: SseDecoder::default(), bytes: 0, done: false, started: false }
    }

    pub(super) fn started(&self) -> bool { self.started }

    pub(super) fn push(&mut self, bytes: &[u8], model: &str, token: &str) -> Result<bool> {
        self.bytes = self.bytes.saturating_add(bytes.len());
        if self.bytes > 64 * 1024 * 1024 {
            return Err(Error::coded("小浣熊单次流式响应超过 64 MiB", "plugin_resource_limit"));
        }
        let events = self.decoder.push(bytes)?;
        self.emit(events, model, token)?;
        if self.done {
            // 上游真实DONE已到；仅冲刷同一网络片段中的尾部，不能补造成功标记。
            let events = self.decoder.push(b"\n\n")?;
            self.emit(events, model, token)?;
            sdk::stream_bytes(b"data: [DONE]\n\n")?;
        }
        Ok(self.done)
    }

    pub(super) fn finish(&mut self, model: &str, token: &str) -> Result<()> {
        let events = self.decoder.push(b"\n\n")?;
        self.emit(events, model, token)?;
        if !self.done {
            return Err(Error::http(502, "小浣熊流提前中断，未收到完整结束标记"));
        }
        sdk::stream_bytes(b"data: [DONE]\n\n")
    }

    fn emit(&mut self, events: Vec<String>, model: &str, token: &str) -> Result<()> {
        for event in events {
            if self.done {
                return Err(Error::http(502, "小浣熊结束标记之后出现额外事件"));
            }
            self.done = super::chat::emit(&event, model, token, &mut self.started)?;
        }
        Ok(())
    }
}
