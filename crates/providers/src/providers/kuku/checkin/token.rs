//! 活动凭证接口可能返回 `jsToken: null`，同会话的活动页仍会内联该凭证。
//! 只解码文本并读取明确赋给 jsToken 的十六进制值，绝不执行上游脚本。

use serde_json::Value;

use crate::http::AccountRequest;

use super::{http, ACTIVITY_REFERER, BASE_URL};

const MAX_TEXT_BYTES: usize = 1024 * 1024;

pub(super) async fn fetch(headers: &[(String, String)]) -> Result<String, String> {
    let url = format!("{BASE_URL}/operation/activitys/api/getJsToken");
    let api_problem = match http::get_json_value(&url, headers, None, Some(30_000)).await {
        Ok(value) => {
            let errno = value.get("errno").and_then(Value::as_i64);
            if errno.is_some_and(|code| code != 0) {
                format!("凭证接口返回 errno={}", errno.unwrap_or_default())
            } else {
                let raw = value.pointer("/data/jsToken").or_else(|| value.get("jsToken"))
                    .and_then(Value::as_str).filter(|text| !text.trim().is_empty());
                match raw {
                    Some(raw) => match extract(raw) {
                        Some(token) => return Ok(token),
                        None => "凭证接口返回的 jsToken 格式无法识别".to_string(),
                    },
                    None => "凭证接口未返回 jsToken".to_string(),
                }
            }
        }
        Err(error) if error.status_code == 401 => {
            return Err(format!("签到凭证获取失败：{}", error.message));
        }
        Err(error) => format!("凭证接口请求失败：{}", error.message),
    };
    let html = activity_page(headers).await
        .map_err(|message| format!("签到凭证获取失败（{api_problem}；{message}）"))?;
    extract(&html).ok_or_else(|| {
        format!("签到凭证解析失败（{api_problem}；活动页未找到可识别的 jsToken）")
    })
}

async fn activity_page(headers: &[(String, String)]) -> Result<String, String> {
    let client = crate::http::control_client(None, "direct");
    let mut request = client.get(ACTIVITY_REFERER)
        .header("Accept", "text/html,application/xhtml+xml")
        .timeout(std::time::Duration::from_secs(30));
    for (key, value) in headers {
        request = request.header(key, value);
    }
    let mut response = request.with_account_user_agent().send().await
        .map_err(|error| format!("活动页读取失败：{}", crate::http::describe(&error)))?;
    if response.url().host_str() != Some("kuku.baidu.com") {
        return Err("活动页跳转到其他站点，请确认该账号已登录库库".to_string());
    }
    if !response.status().is_success() {
        return Err(format!("活动页返回 HTTP {}", response.status().as_u16()));
    }
    if response.content_length().is_some_and(|size| size > MAX_TEXT_BYTES as u64) {
        return Err("活动页响应过大，未解析签到凭证".to_string());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await
        .map_err(|error| format!("活动页读取失败：{}", crate::http::describe(&error)))? {
        if chunk.len() > MAX_TEXT_BYTES.saturating_sub(body.len()) {
            return Err("活动页响应过大，未解析签到凭证".to_string());
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|_| "活动页不是有效的 UTF-8 文本".to_string())
}

fn extract(raw: &str) -> Option<String> {
    if raw.len() > MAX_TEXT_BYTES {
        return None;
    }
    let mut text = raw.trim().to_string();
    for _ in 0..=3 {
        if valid_token(&text) {
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
        return fragment_token(text);
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
        if let Some(token) = fragment_token(&text[body..end]) {
            return Some(token);
        }
        offset = end + "</script".len();
    }
    None
}

fn fragment_token(script: &str) -> Option<String> {
    for (position, _) in script.match_indices("jsToken") {
        if position > 0 && identifier_byte(script.as_bytes()[position - 1]) {
            continue;
        }
        let Some(value) = script[position + "jsToken".len()..].trim_start().strip_prefix('=') else {
            continue;
        };
        if let Some((token, _)) = token_literal(value) {
            return Some(token.to_string());
        }
        let Some((variable, _)) = identifier(value) else { continue };
        let Some(start) = script[..position].rfind("function") else { continue };
        let Some((name, rest)) = identifier(&script[start + "function".len()..position]) else { continue };
        let Some(arguments) = rest.trim_start().strip_prefix('(') else { continue };
        let Some((parameter, rest)) = identifier(arguments) else { continue };
        if parameter != variable { continue; }
        let Some(body) = rest.trim_start().strip_prefix(')')
            .and_then(|rest| rest.trim_start().strip_prefix('{')) else { continue };
        if body.contains('}') { continue; }
        if let Some(token) = call_token(&script[position..], name) {
            return Some(token);
        }
    }
    None
}

fn call_token(script: &str, name: &str) -> Option<String> {
    for (position, _) in script.match_indices(name) {
        if position > 0 && (identifier_byte(script.as_bytes()[position - 1]) || script.as_bytes()[position - 1] == b'.') {
            continue;
        }
        let Some(argument) = script[position + name.len()..].trim_start().strip_prefix('(') else { continue };
        let Some((token, rest)) = token_literal(argument) else { continue };
        if rest.trim_start().starts_with(')') {
            return Some(token.to_string());
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
    let end = text.bytes().take_while(|byte| identifier_byte(*byte)).count();
    Some((&text[..end], &text[end..]))
}

fn identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

fn token_literal(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    let quote = *text.as_bytes().first()?;
    if !matches!(quote, b'\'' | b'"' | b'`') { return None; }
    let rest = &text[1..];
    let end = rest.find(quote as char)?;
    let token = &rest[..end];
    valid_token(token).then_some((token, &rest[end + 1..]))
}

fn valid_token(token: &str) -> bool {
    (64..=256).contains(&token.len()) && token.len() % 2 == 0
        && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_value(bytes[index + 1]), hex_value(bytes[index + 2])) {
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

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
