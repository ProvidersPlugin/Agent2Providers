use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use base64::{engine::general_purpose::STANDARD, Engine};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use super::account;

const MASTER_KEY: [u8; 32] = [
    0xe9, 0x72, 0x50, 0xf0, 0x53, 0x03, 0x16, 0x2c,
    0x8e, 0xcd, 0x68, 0xc6, 0x88, 0xb2, 0xf5, 0x5c,
    0x1d, 0x81, 0xe5, 0x08, 0xd2, 0x43, 0xd8, 0x84,
    0x66, 0x47, 0x2e, 0x7f, 0x54, 0x63, 0x71, 0x23,
];
pub const CLIENT_VERSION: &str = "5.2.7";

fn mac(key: &[u8], bytes: &[u8]) -> Result<Vec<u8>> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key)
        .map_err(|_| Error::new("AtomCode 请求签名初始化失败"))?;
    mac.update(bytes);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn headers(url: &str, token: &str, body: &[u8], mut headers: Value) -> Result<Value> {
    let parsed = url::Url::parse(url).map_err(|_| Error::http(400, "AtomCode 网关地址无效"))?;
    if parsed.origin().ascii_serialization() != "https://llm-api.atomgit.com"
        || parsed.path() != "/v1/chat/completions"
        || parsed.query().is_some() || parsed.fragment().is_some()
        || !parsed.username().is_empty() || parsed.password().is_some()
    {
        return Err(Error::http(400, "AtomCode 对话仅支持已声明的官方签名网关，未将账号令牌发送到其他来源")
            .with_code("atomcode_gateway_invalid"));
    }
    let record = sdk::account()?;
    let secret = &record["secret"];
    let saved = account::text(secret, &["accessToken", "access_token", "token"]);
    let user_id = account::user_id(secret);
    if saved != token || user_id.is_empty() {
        return Err(Error::http(401, "AtomCode 签名所需账号身份或凭据已变化，请重新登录")
            .with_code("atomcode_signing_identity"));
    }
    let timestamp = sdk::host("clock.now", Value::Null)?.as_i64()
        .filter(|time| *time > 0).ok_or_else(|| Error::new("AtomCode 未取得有效签名时间"))? / 1000;
    let random = sdk::host("random.bytes", json!({ "length": 16 }))?;
    let nonce = STANDARD.decode(random.as_str().unwrap_or(""))
        .map_err(|_| Error::new("AtomCode 签名随机数无效"))?;
    if nonce.len() != 16 { return Err(Error::new("AtomCode 签名随机数长度无效")); }
    let nonce = hex(&nonce);
    let mut salt = user_id.into_bytes();
    salt.push(1);
    salt.extend_from_slice(&((timestamp / 3600) as u64).to_le_bytes());
    salt.extend_from_slice(&Sha256::digest(token.as_bytes()));
    salt.extend_from_slice(&Sha256::digest(CLIENT_VERSION.as_bytes()));
    let extracted = mac(&salt, &MASTER_KEY)?;
    let signing_key = mac(&extracted, b"atomcode-signing-v1\x01")?;
    let canonical = format!("v1\nPOST\n{}\n{timestamp}\n{nonce}\n{}", parsed.path(), hex(&Sha256::digest(body)));
    let signature = hex(&mac(&signing_key, canonical.as_bytes())?);
    let fields = headers.as_object_mut().ok_or_else(|| Error::new("AtomCode 请求头无效"))?;
    fields.retain(|name, _| !name.eq_ignore_ascii_case("user-agent") && !name.to_ascii_lowercase().starts_with("x-atomcode-"));
    fields.insert("user-agent".into(), json!(format!("atomcode/{CLIENT_VERSION}")));
    fields.insert("X-AtomCode-Sig".into(), json!(format!("v1:{signature}")));
    fields.insert("X-AtomCode-Ts".into(), json!(timestamp.to_string()));
    fields.insert("X-AtomCode-Nonce".into(), json!(nonce));
    fields.insert("X-AtomCode-Alg".into(), json!("1"));
    fields.insert("X-AtomCode-Ver".into(), json!(CLIENT_VERSION));
    Ok(headers)
}
