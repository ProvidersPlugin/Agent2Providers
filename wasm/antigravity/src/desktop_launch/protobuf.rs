use agent2api_wasm_sdk::{json, Error, Result, Value};
use base64::{engine::general_purpose::STANDARD, Engine};

fn number(mut value: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        bytes.push((value as u8 & 0x7f) | if value > 0x7f { 0x80 } else { 0 });
        value >>= 7;
        if value == 0 { break; }
    }
    bytes
}

fn data(field: u32, bytes: &[u8]) -> Vec<u8> {
    let mut out = number(((field << 3) | 2) as u64);
    out.extend(number(bytes.len() as u64)); out.extend(bytes); out
}

fn text(field: u32, value: &str) -> Vec<u8> { data(field, value.as_bytes()) }

fn scalar(field: u32, value: u64) -> Vec<u8> {
    let mut out = number((field << 3) as u64); out.extend(number(value)); out
}

fn varint(bytes: &[u8], offset: &mut usize) -> Result<u64> {
    let mut result = 0u64;
    for shift in (0..=63).step_by(7) {
        let byte = *bytes.get(*offset).ok_or_else(|| Error::http(409, "客户端账号记录不完整"))?;
        *offset += 1;
        if shift == 63 && byte > 1 { return Err(Error::http(409, "客户端账号记录格式不支持")); }
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 { return Ok(result); }
    }
    Err(Error::http(409, "客户端账号记录格式不支持"))
}

fn field<'a>(bytes: &'a [u8], offset: &mut usize) -> Result<(u32, u8, &'a [u8])> {
    let tag = varint(bytes, offset)?;
    let wire = (tag & 7) as u8;
    let content = match wire {
        0 => { let start = *offset; varint(bytes, offset)?; &bytes[start..*offset] },
        1 | 5 => { let start = *offset; *offset += if wire == 1 { 8 } else { 4 }; bytes.get(start..*offset).ok_or_else(|| Error::http(409, "客户端账号记录不完整"))? },
        2 => { let length = usize::try_from(varint(bytes, offset)?).map_err(|_| Error::new("客户端记录过大"))?; let start = *offset;
            *offset = start.checked_add(length).ok_or_else(|| Error::new("客户端记录过大"))?;
            bytes.get(start..*offset).ok_or_else(|| Error::http(409, "客户端账号记录不完整"))? },
        _ => return Err(Error::http(409, "客户端账号记录格式不支持")),
    };
    if tag >> 3 == 0 || tag >> 3 > 0x1fff_ffff { return Err(Error::http(409, "客户端账号记录格式不支持")); }
    Ok(((tag >> 3) as u32, wire, content))
}

fn entry(key: &str, payload: &[u8]) -> Vec<u8> {
    let row = text(1, &STANDARD.encode(payload));
    data(1, &[text(1, key), data(2, &row)].concat())
}

fn replace(previous: Option<&str>, key: &str, payload: &[u8]) -> Result<String> {
    edit(previous, key, Some(payload))
}

fn remove(previous: Option<&str>, key: &str) -> Result<Option<String>> {
    let value = edit(previous, key, None)?;
    Ok((!value.is_empty()).then_some(value))
}

fn edit(previous: Option<&str>, key: &str, payload: Option<&[u8]>) -> Result<String> {
    let bytes = previous.map(|value| STANDARD.decode(value)).transpose().map_err(|_| Error::http(409, "客户端账号记录不是支持的编码"))?.unwrap_or_default();
    let mut result = Vec::new(); let mut offset = 0; let mut matched = false;
    let mut keys = std::collections::HashSet::new();
    while offset < bytes.len() {
        let start = offset;
        let (id, wire, content) = field(&bytes, &mut offset)?;
        let mut remove = false;
        if id == 1 && wire == 2 {
            let mut row_offset = 0;
            let mut names = Vec::new();
            let mut row_values = Vec::new();
            while row_offset < content.len() {
                let (id, wire, value) = field(content, &mut row_offset)?;
                match (id, wire) {
                    (1, 2) => names.push(std::str::from_utf8(value).map_err(|_| Error::http(409, "客户端账号索引无效"))?),
                    (2, 2) => row_values.push(value),
                    _ => return Err(Error::http(409, "客户端账号索引格式不支持")),
                }
            }
            if names.len() != 1 || row_values.len() != 1 || !keys.insert(names[0].to_string()) {
                return Err(Error::http(409, "客户端账号索引格式不支持"));
            }
            let mut row = 0;
            let (row_id, row_wire, encoded) = field(row_values[0], &mut row)?;
            if row_id != 1 || row_wire != 2 || row != row_values[0].len()
                || STANDARD.decode(encoded).is_err() { return Err(Error::http(409, "客户端账号记录不是支持的编码")); }
            if names[0] == key {
                if matched { return Err(Error::http(409, "客户端账号记录冲突，未覆盖")); }
                matched = true; remove = true;
            }
        }
        if id == 1 && wire != 2 { return Err(Error::http(409, "客户端账号索引格式不支持")); }
        if !remove { result.extend(&bytes[start..offset]); }
    }
    if let Some(payload) = payload { result.extend(entry(key, payload)); }
    Ok(STANDARD.encode(result))
}

pub fn state(previous: &Value, access: &str, refresh: &str, expiry: i64, email: &str, project: &str, id_token: &str) -> Result<Value> {
    if !previous.is_object() || expiry <= 0 { return Err(Error::http(409, "客户端账号状态不可确认")); }
    let timestamp = [scalar(1, expiry as u64), scalar(2, 0)].concat();
    let mut token = [text(1, access), text(2, "Bearer"), text(3, refresh), data(4, &timestamp)].concat();
    if !id_token.is_empty() { token.extend(text(5, id_token)); }
    let user = [text(3, email), text(7, email)].concat();
    let oauth = replace(previous["antigravityUnifiedStateSync.oauthToken"].as_str(), "oauthTokenInfoSentinelKey", &token)?;
    let status = replace(previous["antigravityUnifiedStateSync.userStatus"].as_str(), "userStatusSentinelKey", &user)?;
    let project = if project.is_empty() {
        json!(remove(previous["antigravityUnifiedStateSync.enterprisePreferences"].as_str(), "enterpriseGcpProjectId")?)
    } else {
        json!(replace(previous["antigravityUnifiedStateSync.enterprisePreferences"].as_str(), "enterpriseGcpProjectId", &text(3, project))?)
    };
    Ok(json!({ "antigravityUnifiedStateSync.oauthToken": oauth, "antigravityUnifiedStateSync.userStatus": status,
        "antigravityUnifiedStateSync.enterprisePreferences": project, "antigravityOnboarding": "true",
        "jetskiStateSync.agentManagerInitState": null }))
}
