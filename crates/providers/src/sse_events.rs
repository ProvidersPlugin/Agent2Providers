#[derive(Default)]
pub(crate) struct EventDecoder {
    line: Vec<u8>,
    data: Vec<u8>,
    line_bytes: usize,
    event_bytes: usize,
    skipping_event: bool,
    skip_lf: bool,
    first_line_seen: bool,
}

impl EventDecoder {
    const MAX_EVENT_BYTES: usize = 1024 * 1024;

    /// 回调返回 true 时停在完整事件边界；不解码半个 UTF-8 字符。
    pub(crate) fn feed(&mut self, bytes: &[u8], mut event: impl FnMut(&[u8]) -> bool) -> usize {
        for (index, &byte) in bytes.iter().enumerate() {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' { continue; }
            }
            if byte == b'\r' || byte == b'\n' {
                let stop = self.consume_line(&mut event);
                self.skip_lf = byte == b'\r';
                if stop {
                    return index + 1 + usize::from(byte == b'\r' && bytes.get(index + 1) == Some(&b'\n'));
                }
            } else {
                self.line_bytes = self.line_bytes.saturating_add(1);
                self.event_bytes = self.event_bytes.saturating_add(1);
                if self.event_bytes > Self::MAX_EVENT_BYTES {
                    self.line.clear();
                    self.data.clear();
                    self.skipping_event = true;
                }
                if !self.skipping_event { self.line.push(byte); }
            }
        }
        bytes.len()
    }

    /// EOF 的最后一段只用于读取完整 JSON，不能据此把未结束的 DONE 当成协议完成。
    pub(crate) fn finish(&mut self, mut event: impl FnMut(&[u8]) -> bool) {
        if self.line_bytes > 0 { self.consume_line(&mut event); }
        if !self.skipping_event && !self.data.is_empty() {
            event(self.data.strip_suffix(b"\n").unwrap_or(&self.data));
        }
        self.line.clear();
        self.data.clear();
        self.line_bytes = 0;
        self.event_bytes = 0;
        self.skipping_event = false;
        self.skip_lf = false;
    }

    fn consume_line(&mut self, event: &mut impl FnMut(&[u8]) -> bool) -> bool {
        if self.line_bytes > 0 {
            self.event_bytes = self.event_bytes.saturating_add(1);
            if self.event_bytes > Self::MAX_EVENT_BYTES {
                self.line.clear();
                self.data.clear();
                self.skipping_event = true;
            }
        }
        let mut stop = false;
        if self.line_bytes == 0 {
            if !self.skipping_event && !self.data.is_empty() {
                stop = event(self.data.strip_suffix(b"\n").unwrap_or(&self.data));
            }
            self.data.clear();
            self.event_bytes = 0;
            self.skipping_event = false;
        } else if !self.skipping_event {
            let line = if !self.first_line_seen {
                self.line.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&self.line)
            } else { &self.line };
            let payload = line.strip_prefix(b"data:").or_else(|| (line == b"data").then_some(&[][..]));
            if let Some(payload) = payload {
                self.data.extend_from_slice(payload.strip_prefix(b" ").unwrap_or(payload));
                self.data.push(b'\n');
            }
        }
        self.first_line_seen = true;
        self.line.clear();
        self.line_bytes = 0;
        stop
    }
}

#[derive(Default)]
pub(crate) struct ByteLines {
    pending: Vec<u8>,
    skip_lf: bool,
    first_line_seen: bool,
}

impl ByteLines {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for &byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' { continue; }
            }
            if byte == b'\r' || byte == b'\n' {
                let raw = std::mem::take(&mut self.pending);
                let raw = if self.first_line_seen { raw.as_slice() }
                    else { raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&raw) };
                if let Ok(line) = std::str::from_utf8(raw) { lines.push(line.to_string()); }
                self.first_line_seen = true;
                self.skip_lf = byte == b'\r';
            } else {
                self.pending.push(byte);
            }
        }
        lines
    }

    pub(crate) fn finish(&mut self) -> Option<String> {
        let raw = std::mem::take(&mut self.pending);
        if raw.is_empty() { return None; }
        std::str::from_utf8(&raw).ok().map(str::to_string)
    }
}

#[derive(Default)]
pub(crate) struct DataEvents {
    decoder: EventDecoder,
    pending: Vec<u8>,
}

impl DataEvents {
    pub(crate) fn new() -> Self { Self::default() }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let previous = self.pending.len();
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        let mut offset = 0;
        let mut drained = 0;
        while offset < bytes.len() {
            let mut complete = false;
            offset += self.decoder.feed(&bytes[offset..], |data| {
                complete = true;
                if let Ok(text) = std::str::from_utf8(data) { events.push(text.to_string()); }
                true
            });
            if complete { drained = previous + offset; } else { break; }
        }
        self.pending.drain(..drained);
        if self.pending.len() > EventDecoder::MAX_EVENT_BYTES { self.pending.clear(); }
        events
    }

    pub(crate) fn finish(&mut self) -> Vec<String> {
        let mut events = Vec::new();
        self.decoder.finish(|data| {
            if !is_done(data) {
                if let Ok(text) = std::str::from_utf8(data) { events.push(text.to_string()); }
            }
            false
        });
        self.pending.clear();
        events
    }

    pub(crate) fn take_pending(&mut self) -> Vec<u8> {
        self.decoder = EventDecoder::default();
        std::mem::take(&mut self.pending)
    }
}

pub(crate) fn is_done(data: &[u8]) -> bool {
    std::str::from_utf8(data).is_ok_and(|text| text.trim() == "[DONE]")
}

pub(crate) fn data_of(event: &[u8]) -> Option<Vec<u8>> {
    let mut decoder = EventDecoder::default();
    let mut data = None;
    decoder.feed(event, |payload| { data = Some(payload.to_vec()); false });
    data
}

pub(crate) fn event_end(bytes: &[u8]) -> Option<usize> {
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' || bytes[index] == b'\n' {
            let end = index + 1 + usize::from(bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n'));
            if index == start { return Some(end); }
            start = end;
            index = end;
        } else { index += 1; }
    }
    None
}

pub(crate) fn lines_inclusive(mut text: &str) -> impl Iterator<Item = &str> {
    std::iter::from_fn(move || {
        if text.is_empty() { return None; }
        let bytes = text.as_bytes();
        let end = bytes.iter().position(|byte| *byte == b'\r' || *byte == b'\n')
            .map(|index| index + 1 + usize::from(bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n')))
            .unwrap_or(text.len());
        let (line, rest) = text.split_at(end);
        text = rest;
        Some(line)
    })
}
