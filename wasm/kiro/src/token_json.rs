//! Kiro token响应原字节的有界严格JSON；不增加serde依赖，不参与网络或重试。
//! 先核完整结构、解码后的重复键和16层容器，再由serde_json读取同一份字节。

use std::collections::BTreeSet;
use std::ops::Range;

use agent2api_wasm_sdk::{Error, Result, Value};

pub(crate) const LIMIT: usize = 128 * 1024;
const MAX_DEPTH: usize = 16;

fn invalid() -> Error {
    Error::http(
        502,
        "Kiro授权响应格式无效、字段重复或超过限制，未接受本次凭据",
    )
    .with_code("kiro_token_response_invalid")
}

pub(crate) fn parse(bytes: &[u8]) -> Result<Value> {
    if bytes.is_empty() || bytes.len() > LIMIT || std::str::from_utf8(bytes).is_err() {
        return Err(invalid());
    }
    let mut scan = Scanner { bytes, offset: 0 };
    scan.space();
    if scan.peek() != Some(b'{') {
        return Err(invalid());
    }
    scan.value(0)?;
    scan.space();
    if scan.offset != bytes.len() {
        return Err(invalid());
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    if !value.is_object() {
        return Err(invalid());
    }
    Ok(value)
}

struct Scanner<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl Scanner<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.offset).copied()
    }
    fn space(&mut self) {
        while self
            .peek()
            .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        {
            self.offset += 1;
        }
    }
    fn expect(&mut self, byte: u8) -> Result<()> {
        if self.peek() != Some(byte) {
            return Err(invalid());
        }
        self.offset += 1;
        Ok(())
    }
    fn value(&mut self, depth: usize) -> Result<()> {
        self.space();
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(|_| ()),
            Some(_) => self.scalar(),
            None => Err(invalid()),
        }
    }
    fn object(&mut self, depth: usize) -> Result<()> {
        if depth >= MAX_DEPTH {
            return Err(invalid());
        }
        self.expect(b'{')?;
        self.space();
        if self.peek() == Some(b'}') {
            self.offset += 1;
            return Ok(());
        }
        let mut keys = BTreeSet::new();
        loop {
            self.space();
            let range = self.string()?;
            let key: String = serde_json::from_slice(&self.bytes[range]).map_err(|_| invalid())?;
            if !keys.insert(key) {
                return Err(invalid());
            }
            self.space();
            self.expect(b':')?;
            self.value(depth + 1)?;
            self.space();
            match self.peek() {
                Some(b'}') => {
                    self.offset += 1;
                    return Ok(());
                }
                Some(b',') => self.offset += 1,
                _ => return Err(invalid()),
            }
        }
    }
    fn array(&mut self, depth: usize) -> Result<()> {
        if depth >= MAX_DEPTH {
            return Err(invalid());
        }
        self.expect(b'[')?;
        self.space();
        if self.peek() == Some(b']') {
            self.offset += 1;
            return Ok(());
        }
        loop {
            self.value(depth + 1)?;
            self.space();
            match self.peek() {
                Some(b']') => {
                    self.offset += 1;
                    return Ok(());
                }
                Some(b',') => self.offset += 1,
                _ => return Err(invalid()),
            }
        }
    }
    fn string(&mut self) -> Result<Range<usize>> {
        let begin = self.offset;
        self.expect(b'"')?;
        while let Some(byte) = self.peek() {
            match byte {
                b'"' => {
                    self.offset += 1;
                    return Ok(begin..self.offset);
                }
                b'\\' => {
                    self.offset += 1;
                    if self.peek().is_none() {
                        return Err(invalid());
                    }
                    self.offset += 1;
                }
                0..=31 => return Err(invalid()),
                _ => self.offset += 1,
            }
        }
        Err(invalid())
    }
    fn scalar(&mut self) -> Result<()> {
        let begin = self.offset;
        while self
            .peek()
            .is_some_and(|byte| !matches!(byte, b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n'))
        {
            self.offset += 1;
        }
        if self.offset == begin {
            return Err(invalid());
        }
        let value: Value =
            serde_json::from_slice(&self.bytes[begin..self.offset]).map_err(|_| invalid())?;
        if !matches!(value, Value::Null | Value::Bool(_) | Value::Number(_)) {
            return Err(invalid());
        }
        Ok(())
    }
}
