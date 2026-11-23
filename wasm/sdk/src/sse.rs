use crate::{Error, Result};

#[derive(Default)]
pub struct SseDecoder {
    line: Vec<u8>,
    data: Vec<u8>,
    skip_lf: bool,
}

impl SseDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>> {
        let mut events = Vec::new();
        for byte in bytes {
            if self.skip_lf { self.skip_lf = false; if *byte == b'\n' { continue; } }
            if matches!(*byte, b'\r' | b'\n') {
                self.line(&mut events)?;
                self.skip_lf = *byte == b'\r';
            } else {
                if self.line.len() + self.data.len() >= 1024 * 1024 { return Err(Error::new("上游单个流式事件超过 1 MiB")); }
                self.line.push(*byte);
            }
        }
        Ok(events)
    }

    fn line(&mut self, events: &mut Vec<String>) -> Result<()> {
        if self.line.is_empty() {
            if !self.data.is_empty() {
                let data = std::mem::take(&mut self.data);
                events.push(String::from_utf8(data).map_err(|_| Error::new("上游流式事件不是有效 UTF-8"))?);
            }
        } else if let Some(value) = self.line.strip_prefix(b"data:") {
            let value = value.strip_prefix(b" ").unwrap_or(value);
            if !self.data.is_empty() { self.data.push(b'\n'); }
            self.data.extend_from_slice(value);
        }
        self.line.clear();
        Ok(())
    }
}
