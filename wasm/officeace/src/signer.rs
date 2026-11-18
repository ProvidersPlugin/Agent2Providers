//! 保留旧 SDK/V11 的规范化差异：RFC3986 编码、WHATWG query、V11 派生 hex 密钥。
//! 请求日期只取 SDK clock；签名正文就是 http.open_raw 发送的紧凑 JSON/表单原字节。

use super::{endpoints, util};
use agent2api_wasm_sdk::{self as sdk, Error, Result, Value};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::BTreeMap;

#[derive(Clone, Default)]
pub struct Credential {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub security_token: String,
    pub project_id: String,
}

#[derive(Clone, Copy)]
pub enum Algorithm {
    Sdk,
    V11,
}

fn decode(raw: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    let input = raw.as_bytes();
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' && i + 2 < input.len() {
            let hi = char::from(input[i + 1]).to_digit(16);
            let lo = char::from(input[i + 2]).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                bytes.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        bytes.push(input[i]);
        i += 1;
    }
    bytes
}

fn encode_bytes(bytes: &[u8]) -> String {
    let mut out = String::new();
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(*byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn canonical_path(path: &str) -> String {
    let mut path = path
        .split('/')
        .map(|part| encode_bytes(&decode(part)))
        .collect::<Vec<_>>()
        .join("/");
    if !path.ends_with('/') {
        path.push('/');
    }
    path
}

fn canonical_query(url: &url::Url) -> String {
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", util::encode(key), util::encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn hmac(key: &[u8], message: &[u8]) -> Result<Vec<u8>> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key).map_err(|_| Error::new("OfficeAce 签名初始化失败"))?;
    mac.update(message);
    Ok(mac.finalize().into_bytes().to_vec())
}

pub fn sign(
    credential: &Credential,
    algorithm: Algorithm,
    method: &str,
    raw_url: &str,
    body: &[u8],
    extra: &Value,
) -> Result<Value> {
    if credential.access_key_id.is_empty() || credential.secret_access_key.is_empty() {
        return Err(Error::http(400, "OfficeAce 控制面 AK/SK 不完整"));
    }
    let url = url::Url::parse(raw_url).map_err(|_| Error::http(400, "OfficeAce 签名地址无效"))?;
    let host = url
        .host_str()
        .ok_or_else(|| Error::http(400, "OfficeAce 签名地址缺少主机"))?;
    let host = url
        .port()
        .map(|port| format!("{host}:{port}"))
        .unwrap_or_else(|| host.into());
    let date = chrono::DateTime::from_timestamp_millis(sdk::now())
        .ok_or_else(|| Error::new("主程序时间无效，不能签名"))?
        .format("%Y%m%dT%H%M%SZ")
        .to_string();
    let mut headers: BTreeMap<String, String> = extra
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| {
            value
                .as_str()
                .map(|value| (key.to_ascii_lowercase(), value.to_string()))
        })
        .collect();
    // 不签 user-agent，宿主会按账号覆盖它；签不同字节会导致无法辨认的 401。
    headers.remove("user-agent");
    headers.remove("authorization");
    headers.insert("host".into(), host);
    headers.insert("x-sdk-date".into(), date.clone());
    if !credential.security_token.is_empty() {
        headers.insert("x-security-token".into(), credential.security_token.clone());
    }
    if !credential.project_id.is_empty() {
        headers.insert("x-project-id".into(), credential.project_id.clone());
    }
    let signed = headers
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers: String = headers
        .iter()
        .map(|(key, value)| {
            format!(
                "{key}:{}\n",
                value.split_whitespace().collect::<Vec<_>>().join(" ")
            )
        })
        .collect();
    let canonical = [
        method.to_ascii_uppercase(),
        canonical_path(url.path()),
        canonical_query(&url),
        canonical_headers,
        signed.clone(),
        util::sha256(body),
    ]
    .join("\n");
    let digest = util::sha256(canonical.as_bytes());
    let auth = match algorithm {
        Algorithm::Sdk => {
            let input = format!("SDK-HMAC-SHA256\n{date}\n{digest}");
            let signature = util::hex(&hmac(
                credential.secret_access_key.as_bytes(),
                input.as_bytes(),
            )?);
            format!(
                "SDK-HMAC-SHA256 Access={}, SignedHeaders={signed}, Signature={signature}",
                credential.access_key_id
            )
        }
        Algorithm::V11 => {
            let scope = format!("{}/{}/apic", &date[..8], endpoints::REGION);
            let input = format!("V11-HMAC-SHA256\n{date}\n{scope}\n{digest}");
            let first = hmac(
                credential.access_key_id.as_bytes(),
                credential.secret_access_key.as_bytes(),
            )?;
            let mut seed = scope.as_bytes().to_vec();
            seed.push(1);
            let derived = util::hex(&hmac(&first, &seed)?);
            let signature = util::hex(&hmac(derived.as_bytes(), input.as_bytes())?);
            format!("V11-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}", credential.access_key_id)
        }
    };
    headers.insert("authorization".into(), auth);
    Ok(serde_json::to_value(headers).map_err(|_| Error::new("OfficeAce 签名头序列化失败"))?)
}
