//! ES256 DPoP，沿用旧 {private_key_jwk, public_key_jwk} 落盘形状。
//! P-256/RFC6979 均交给 RustCrypto；关闭 p256 默认/std 特性，随机仅由 SDK 注入。

use super::util;
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use p256::ecdsa::{signature::hazmat::PrehashSigner, Signature, SigningKey};
use sha2::{Digest, Sha256};

pub struct DpopKey {
    signing: SigningKey,
    public: Value,
}

impl DpopKey {
    pub fn from_json(pair: &Value) -> Result<Self> {
        let private = pair
            .get("private_key_jwk")
            .or_else(|| pair.get("privateKeyJwk"))
            .ok_or_else(|| Error::http(400, "DPoP 私钥缺失，请重新网页登录"))?;
        if private["kty"] != "EC" || private["crv"] != "P-256" {
            return Err(Error::http(400, "DPoP 私钥必须使用 EC / P-256"));
        }
        let scalar = private["d"].as_str().unwrap_or("").trim();
        let bytes = URL_SAFE_NO_PAD
            .decode(scalar)
            .map_err(|_| Error::http(400, "DPoP 私钥编码无效"))?;
        if bytes.len() != 32 {
            return Err(Error::http(400, "DPoP 私钥长度无效"));
        }
        let signing = SigningKey::from_slice(&bytes)
            .map_err(|_| Error::http(400, "DPoP 私钥不是有效 P-256 标量"))?;
        let public = public_of(&signing)?;
        for stored in [
            Some(private),
            pair.get("public_key_jwk")
                .or_else(|| pair.get("publicKeyJwk")),
        ]
        .into_iter()
        .flatten()
        {
            for field in ["x", "y"] {
                if stored
                    .get(field)
                    .is_some_and(|value| value != &public[field])
                {
                    return Err(Error::http(400, "DPoP 公私钥不一致，请重新网页登录"));
                }
            }
        }
        Ok(Self { signing, public })
    }

    pub fn generate() -> Result<Value> {
        for _ in 0..8 {
            let seed = util::random(32)?;
            let Ok(signing) = SigningKey::from_slice(&seed) else {
                continue;
            };
            let public = public_of(&signing)?;
            let mut private = public.clone();
            private["d"] = json!(util::b64url(&seed));
            return Ok(json!({ "private_key_jwk": private, "public_key_jwk": public }));
        }
        Err(Error::new("无法生成安全 P-256 密钥，请重新发起登录"))
    }

    pub fn proof(&self, method: &str, endpoint: &str) -> Result<String> {
        let header = json!({ "alg": "ES256", "typ": "dpop+jwt", "jwk": self.public });
        let payload = json!({ "htm": method.to_ascii_uppercase(), "htu": endpoint,
            "iat": sdk::now() / 1000, "jti": util::random_hex(32)? });
        let input = format!(
            "{}.{}",
            util::b64url(header.to_string().as_bytes()),
            util::b64url(payload.to_string().as_bytes())
        );
        let signature: Signature = self
            .signing
            .sign_prehash(&Sha256::digest(input.as_bytes()))
            .map_err(|_| Error::new("DPoP 签名失败"))?;
        Ok(format!("{input}.{}", util::b64url(&signature.to_bytes())))
    }
}

fn public_of(signing: &SigningKey) -> Result<Value> {
    let point = signing.verifying_key().to_encoded_point(false);
    let x = point.x().ok_or_else(|| Error::new("DPoP 公钥缺少横坐标"))?;
    let y = point.y().ok_or_else(|| Error::new("DPoP 公钥缺少纵坐标"))?;
    Ok(json!({ "kty": "EC", "crv": "P-256", "x": util::b64url(x), "y": util::b64url(y) }))
}
