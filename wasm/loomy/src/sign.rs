//! 复用客户端 CAccount HMAC-SHA1 公式；签名对随官方安装包分发，不是用户凭据。
//! 九段待签串，末两段空行；Content-MD5 覆盖实际发送的同一紧凑 JSON 字节。
//! WASM 不访问环境变量、时钟或随机系统接口，日期/nonce 均由宿主能力提供。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use base64::Engine;
use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use sha1::Sha1;

const ACCESS_KEY_ID: &str = "2thryby66wxi53sk";
const ACCESS_KEY_SECRET: &str = "zsak6eadrbawz683wf5r3m2snrwj868r";

pub fn random_hex(length: usize) -> Result<String> {
    let value = sdk::host("random.bytes", json!({ "length": length }))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.as_str().unwrap_or(""))
        .map_err(|_| Error::new("主程序随机数编码无效"))?;
    if bytes.len() != length {
        return Err(Error::new("主程序随机数字节数无效"));
    }
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn uuid() -> Result<String> {
    let value = sdk::host("random.bytes", json!({ "length": 16 }))?;
    let mut bytes = base64::engine::general_purpose::STANDARD
        .decode(value.as_str().unwrap_or(""))
        .map_err(|_| Error::new("主程序随机数编码无效"))?;
    if bytes.len() != 16 {
        return Err(Error::new("主程序随机数字节数无效"));
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

fn escaped_path(path: &str) -> String {
    let clean = format!("/{}", path.trim_matches('/'));
    clean
        .split('/')
        .map(|segment| {
            let mut value = String::new();
            for byte in segment.bytes() {
                if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                    value.push(char::from(byte));
                } else {
                    value.push_str(&format!("%{byte:02X}"));
                }
            }
            value
        })
        .collect::<Vec<_>>()
        .join("/")
}

pub fn headers(path: &str, body: &[u8]) -> Result<Value> {
    let date = chrono::DateTime::from_timestamp_millis(sdk::now())
        .ok_or_else(|| Error::new("主程序时间无效，无法签名短信请求"))?
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let nonce = uuid()?;
    let md5 = if body.is_empty() {
        String::new()
    } else {
        base64::engine::general_purpose::STANDARD.encode(Md5::digest(body))
    };
    let text = [
        "POST".to_string(),
        escaped_path(path),
        String::new(),
        md5.clone(),
        "application/json".to_string(),
        date.clone(),
        nonce.clone(),
        String::new(),
        String::new(),
    ]
    .join("\n");
    let mut mac = Hmac::<Sha1>::new_from_slice(ACCESS_KEY_SECRET.as_bytes())
        .map_err(|_| Error::new("Loomy 请求签名初始化失败"))?;
    mac.update(text.as_bytes());
    let signature = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    Ok(
        json!({ "Authorization": format!("account {ACCESS_KEY_ID}:{signature}"), "Date": date,
        "Nonce": nonce, "Content-MD5": md5, "Content-Type": "application/json", "Accept": "application/json" }),
    )
}
