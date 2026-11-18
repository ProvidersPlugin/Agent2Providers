//! 原生 trae/device.rs 的 P-256 SPKI 公钥与 PKCS#8 私钥格式。
//! 仅 SDK 提供随机标量；不启用 p256/std、getrandom、WASI，也不生成伪造设备密钥。

use super::util;
use agent2api_wasm_sdk::{json, Error, Result, Value};
use base64::{engine::general_purpose::STANDARD, Engine};
use p256::{
    ecdsa::{signature::Signer, Signature, SigningKey},
    pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey},
};

fn pem(label: &str, bytes: &[u8]) -> String {
    let encoded = STANDARD.encode(bytes);
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in encoded.as_bytes().chunks(64) {
        out.extend(line.iter().map(|byte| char::from(*byte)));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

fn der(label: &str, value: &str) -> Result<Vec<u8>> {
    let invalid = || Error::http(400, "Trae 设备密钥不是有效 PEM");
    if value.len() > 8192 {
        return Err(invalid());
    }
    let value = value
        .trim()
        .strip_prefix(&format!("-----BEGIN {label}-----"))
        .ok_or_else(invalid)?;
    let value = value
        .strip_suffix(&format!("-----END {label}-----"))
        .ok_or_else(invalid)?;
    let encoded: String = value.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    STANDARD.decode(encoded).map_err(|_| invalid())
}

pub fn generate() -> Result<Value> {
    for _ in 0..16 {
        let mut bytes = util::random(32)?;
        let signing = SigningKey::from_slice(&bytes);
        bytes.fill(0);
        let Ok(key) = signing else {
            continue;
        };
        let public = key
            .verifying_key()
            .to_public_key_der()
            .map_err(|_| Error::http(503, "Trae 设备公钥编码失败"))?;
        let private = key
            .to_pkcs8_der()
            .map_err(|_| Error::http(503, "Trae 设备私钥编码失败"))?;
        return Ok(
            json!({ "devicePublicKey": pem("PUBLIC KEY", public.as_bytes()),
            "devicePrivateKey": pem("PRIVATE KEY", private.as_bytes()) }),
        );
    }
    Err(Error::http(503, "Trae 设备密钥生成失败，请重新登录"))
}

pub fn proof(path: &str, client: &str, refresh: &str, private: &str) -> Result<Value> {
    let mut bytes = der("PRIVATE KEY", private)?;
    let key = SigningKey::from_pkcs8_der(&bytes);
    bytes.fill(0);
    let key = key.map_err(|_| Error::http(400, "Trae 设备私钥不是有效 P-256 PKCS#8"))?;
    let timestamp = util::now()? / 1000;
    let nonce = util::random_hex(16)?;
    let mut text = format!("POST\n{path}\n{client}\n{refresh}\n{timestamp}\n{nonce}").into_bytes();
    let signed: Signature = key.sign(&text);
    text.fill(0);
    Ok(
        json!({ "Signature": STANDARD.encode(signed.to_der().as_bytes()),
        "Timestamp": timestamp, "Nonce": nonce }),
    )
}

pub fn validate(public: &str, private: &str) -> Result<()> {
    if public.is_empty() && private.is_empty() {
        return Ok(());
    }
    if public.is_empty() || private.is_empty() {
        return Err(Error::http(
            400,
            "Trae 设备公钥与私钥必须成对提供；不能补一把新密钥冒充旧绑定",
        ));
    }
    let mut private = der("PRIVATE KEY", private)?;
    let key = SigningKey::from_pkcs8_der(&private);
    private.fill(0);
    let key = key.map_err(|_| Error::http(400, "Trae 设备私钥不是有效 P-256 PKCS#8"))?;
    let actual = key
        .verifying_key()
        .to_public_key_der()
        .map_err(|_| Error::http(400, "Trae 设备公钥无法校验"))?;
    if actual.as_bytes() != der("PUBLIC KEY", public)? {
        return Err(Error::http(
            400,
            "Trae 设备公私钥不匹配，请导入原始成对密钥",
        ));
    }
    Ok(())
}
