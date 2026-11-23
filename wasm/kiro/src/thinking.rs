//! `<thinking>`/`</thinking>` 跨片段切分，沿用原生只在开头识别标签的规则。
//! 只缓存可能的标签后缀，连续空白也有上限；不会持有整段回答。

use agent2api_wasm_sdk::{Error, Result};

#[derive(Default)]
pub struct Parser {
    state: u8,
    buffer: String,
    said: bool,
}
pub enum Part {
    Text(String),
    Think(String),
}
const OPEN: &str = "<thinking>";
const CLOSE: &str = "</thinking>";
impl Parser {
    pub fn feed(&mut self, text: &str) -> Result<Vec<Part>> {
        self.buffer.push_str(text);
        let mut out = Vec::new();
        loop {
            match self.state {
                0 => {
                    let trimmed = self.buffer.trim_start_matches([' ', '\t', '\r', '\n']);
                    if trimmed.starts_with(OPEN) {
                        self.buffer = trimmed[OPEN.len()..].to_string();
                        self.state = 1;
                        continue;
                    }
                    if trimmed.len() < OPEN.len() && OPEN.starts_with(trimmed) {
                        if self.buffer.len() > 4096 {
                            return Err(Error::http(502, "Kiro 思考标签前空白超过上限"));
                        }
                        return Ok(out);
                    }
                    self.state = 2;
                }
                1 => {
                    if !self.said {
                        self.buffer = self.buffer.trim_start_matches(['\r', '\n']).to_string();
                        if self.buffer.is_empty() {
                            return Ok(out);
                        }
                    }
                    if let Some(index) = self.buffer.find(CLOSE) {
                        if index > 0 {
                            out.push(Part::Think(self.buffer[..index].into()));
                            self.said = true;
                        }
                        self.buffer = self.buffer[index + CLOSE.len()..]
                            .trim_start_matches(['\r', '\n'])
                            .to_string();
                        self.state = 2;
                        continue;
                    }
                    let hold = (1..CLOSE.len().min(self.buffer.len() + 1))
                        .rev()
                        .find(|length| self.buffer.ends_with(&CLOSE[..*length]))
                        .unwrap_or(0);
                    let keep = self.buffer.len() - hold;
                    if keep > 0 {
                        out.push(Part::Think(self.buffer[..keep].into()));
                        self.buffer = self.buffer[keep..].into();
                        self.said = true;
                    }
                    return Ok(out);
                }
                _ => {
                    if !self.buffer.is_empty() {
                        out.push(Part::Text(std::mem::take(&mut self.buffer)));
                    }
                    return Ok(out);
                }
            }
        }
    }
    pub fn end(&mut self) -> Vec<Part> {
        if self.buffer.is_empty() {
            return Vec::new();
        }
        let value = std::mem::take(&mut self.buffer);
        let part = if self.state == 1 {
            Part::Think(value)
        } else {
            Part::Text(value)
        };
        self.state = 2;
        vec![part]
    }
}
