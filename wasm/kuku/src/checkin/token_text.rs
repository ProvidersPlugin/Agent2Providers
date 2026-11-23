//! 从最新原生checkin/token.rs迁移的纯文本提取部分，不执行任何JavaScript。
//! 只接受明确jsToken赋值/同函数形参传入的64至256位偶数长度十六进制值。

const MAX: usize = 1024 * 1024;
pub fn extract(raw: &str) -> Option<String> {
    if raw.len() > MAX {
        return None;
    }
    let mut text = raw.trim().to_string();
    for _ in 0..=3 {
        if valid(&text) {
            return Some(text);
        }
        if let Some(token) = script_token(&text) {
            return Some(token);
        }
        let decoded = percent_decode(&text)?;
        if decoded == text {
            break;
        }
        text = decoded;
    }
    None
}
fn script_token(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    if !lower.contains("<script") {
        return fragment(text);
    }
    let mut offset = 0;
    while let Some(start) = lower[offset..].find("<script") {
        let start = offset + start;
        let head = start + "<script".len();
        let boundary = *lower.as_bytes().get(head)?;
        if boundary != b'>' && !boundary.is_ascii_whitespace() {
            offset = head;
            continue;
        }
        let body = head + lower[head..].find('>')? + 1;
        let end = body + lower[body..].find("</script")?;
        if let Some(token) = fragment(&text[body..end]) {
            return Some(token);
        }
        offset = end + "</script".len();
    }
    None
}
fn fragment(script: &str) -> Option<String> {
    for (position, _) in script.match_indices("jsToken").take(256) {
        if position > 0 && identifier_byte(script.as_bytes()[position - 1]) {
            continue;
        }
        let Some(value) = script[position + "jsToken".len()..]
            .trim_start()
            .strip_prefix('=')
        else {
            continue;
        };
        if let Some((token, _)) = literal(value) {
            return Some(token.into());
        }
        let Some((variable, _)) = identifier(value) else {
            continue;
        };
        let Some(start) = script[..position].rfind("function") else {
            continue;
        };
        let Some((name, rest)) = identifier(&script[start + "function".len()..position]) else {
            continue;
        };
        let Some(arguments) = rest.trim_start().strip_prefix('(') else {
            continue;
        };
        let Some((parameter, rest)) = identifier(arguments) else {
            continue;
        };
        if parameter != variable {
            continue;
        }
        let Some(body) = rest
            .trim_start()
            .strip_prefix(')')
            .and_then(|rest| rest.trim_start().strip_prefix('{'))
        else {
            continue;
        };
        if body.contains('}') {
            continue;
        }
        if let Some(token) = call(&script[position..], name) {
            return Some(token);
        }
    }
    None
}
fn call(script: &str, name: &str) -> Option<String> {
    for (position, _) in script.match_indices(name).take(1024) {
        if position > 0
            && (identifier_byte(script.as_bytes()[position - 1])
                || script.as_bytes()[position - 1] == b'.')
        {
            continue;
        }
        let Some(argument) = script[position + name.len()..]
            .trim_start()
            .strip_prefix('(')
        else {
            continue;
        };
        let Some((token, rest)) = literal(argument) else {
            continue;
        };
        if rest.trim_start().starts_with(')') {
            return Some(token.into());
        }
    }
    None
}
fn identifier(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    let first = *text.as_bytes().first()?;
    if !first.is_ascii_alphabetic() && first != b'_' && first != b'$' {
        return None;
    }
    let end = text
        .bytes()
        .take_while(|byte| identifier_byte(*byte))
        .count();
    Some((&text[..end], &text[end..]))
}
fn identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}
fn literal(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    let quote = *text.as_bytes().first()?;
    if !matches!(quote, b'\'' | b'"' | b'`') {
        return None;
    }
    let rest = &text[1..];
    let end = rest.find(quote as char)?;
    let token = &rest[..end];
    valid(token).then_some((token, &rest[end + 1..]))
}
fn valid(token: &str) -> bool {
    (64..=256).contains(&token.len())
        && token.len() % 2 == 0
        && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                out.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).ok()
}
fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
