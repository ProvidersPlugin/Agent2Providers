//! 改写自本仓 codearts/signer.rs（AGPL-3.0-only，2026-11-10）。
//! 保留 Go net/url 的逐对 query 规则及 JS encodeURIComponent 未保留字符。

use crate::errors;
use agent2api_wasm_sdk::Result;

fn unhex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode(input: &str, query: bool) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let a = *bytes.get(index + 1)?;
                let b = *bytes.get(index + 2)?;
                out.push((unhex(a)? << 4) | unhex(b)?);
                index += 3;
            }
            b'+' if query => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    Some(out)
}

pub fn component(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &byte in bytes {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub fn form(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(byte as char);
        } else if byte == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub fn canonical_uri(raw_path: &str) -> Result<String> {
    let path =
        decode(raw_path, false).ok_or_else(|| errors::invalid("CodeArts 请求路径转义无效"))?;
    if path.is_empty() {
        return Ok("/".to_string());
    }
    let mut path = path
        .split(|byte| *byte == b'/')
        .map(component)
        .collect::<Vec<_>>()
        .join("/");
    if !path.ends_with('/') {
        path.push('/');
    }
    Ok(path)
}

pub fn canonical_query(raw: &str) -> String {
    let mut pairs = Vec::new();
    for part in raw.split('&') {
        // Go ParseQuery：坏转义或含分号只丢当前一对，无等号按空值处理。
        if part.is_empty() || part.contains(';') {
            continue;
        }
        let (key, value) = part.split_once('=').unwrap_or((part, ""));
        if let (Some(key), Some(value)) = (decode(key, true), decode(value, true)) {
            pairs.push((component(&key), value));
        }
    }
    pairs.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
    pairs
        .iter()
        .map(|(key, value)| format!("{key}={}", component(value)))
        .collect::<Vec<_>>()
        .join("&")
}
