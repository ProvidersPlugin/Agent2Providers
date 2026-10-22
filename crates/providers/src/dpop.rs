//! DPoP proof（RFC 9449 的 `DPoP` 头）：一个 ES256 的 JWS，头部嵌**公钥 JWK**，
//! 载荷声明「这个方法、这个地址、这个时刻、这个一次性 id」。
//!
//! ── 为什么这里还有一份（与 `providers::codearts::dpop` 并存）─────
//! CodeArts 与 OfficeAce 同属华为云身份栈，两家的 proof 算法逐字相同；但两家
//! 的**落盘形态**不同：CodeArts 用强类型 `Jwk` / `DpopKeyPair`（`credentials.rs`），
//! 而 OfficeAce 的密钥对直接以 JSON 存在账号 secret 的 `dpopKeyPair` 键里
//! （网关那版就是这么存的，存量记录不能改形状）。
//!
//! 强行合并会让其中一个模块去依赖另一个 provider 的 feature —— 插件是按家
//! 编 feature 的（`Cargo.toml` 的 `officeace` / `codearts` 可独立开关），那会
//! 让「只装一家」的包编译不过。所以这里放一份**JSON 形态**的通用实现，
//! 谁需要谁用；CodeArts 那份继续服务它自己的强类型凭据。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 本 crate 编译期禁止 unwrap/expect/panic（见 `main.rs` 的 deny）。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use p256::ecdsa::{signature::hazmat::PrehashSigner, Signature, SigningKey, VerifyingKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// 上游只收这一种算法（与 CodeArts 那条同一个 STS）。
const ALG: &str = "ES256";
/// JWS 头的 `typ`。
const TYP: &str = "dpop+jwt";

/// 一份可用的 DPoP 密钥：私钥（签名）+ 公钥 JWK（嵌进 JWS 头）。
pub struct DpopKey {
    signing: SigningKey,
    public_jwk: Value,
}

impl DpopKey {
    /// 从落盘的 `{private_key_jwk, public_key_jwk}` 恢复。
    ///
    /// 只读私钥那一半（公钥由它推出）；落盘的公钥若存在且与私钥不符，
    /// 说明凭据被改坏了 —— 宁可报错，也不要签出一份「头里的公钥和签名的
    /// 私钥对不上」的 proof 去打上游。
    pub fn from_json(pair: &Value) -> Result<Self, String> {
        let private = pair
            .get("private_key_jwk")
            .or_else(|| pair.get("privateKeyJwk"))
            .ok_or_else(|| "DPoP 私钥缺失（dpopKeyPair.private_key_jwk）".to_string())?;
        let kty = private.get("kty").and_then(Value::as_str).unwrap_or("");
        let crv = private.get("crv").and_then(Value::as_str).unwrap_or("");
        let scalar = private.get("d").and_then(Value::as_str).unwrap_or("");
        if kty != "EC" || crv != "P-256" || scalar.trim().is_empty() {
            return Err("DPoP 私钥不完整（需要 EC / P-256 / d）".to_string());
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(scalar.trim())
            .map_err(|_| "DPoP 私钥不是合法的 base64url".to_string())?;
        let signing = SigningKey::from_slice(&bytes)
            .map_err(|_| "DPoP 私钥不是合法的 P-256 标量".to_string())?;
        let public_jwk = public_jwk_of(&signing);
        // 落盘的公钥若在，必须与私钥推出的一致
        if let Some(stored) = pair
            .get("public_key_jwk")
            .or_else(|| pair.get("publicKeyJwk"))
            .filter(|value| value.is_object())
        {
            let stored_x = stored.get("x").and_then(Value::as_str).unwrap_or("");
            let stored_y = stored.get("y").and_then(Value::as_str).unwrap_or("");
            let same = stored_x.trim().is_empty()
                && stored_y.trim().is_empty()
                || (stored_x == text_of(&public_jwk, "x") && stored_y == text_of(&public_jwk, "y"));
            if !same {
                return Err("DPoP 公私钥不一致，请重新登录".to_string());
            }
        }
        Ok(Self { signing, public_jwk })
    }

    /// **新生成**一把 P-256 密钥，返回可直接落盘的那一份。
    ///
    /// 发起登录时用一次：这把私钥要跟着凭据存下来 —— 续期时上游用 proof 头里
    /// 的公钥验签，**换一把就续不了**（与 PKCE verifier 同一条道理）。
    pub fn generate() -> Result<Value, String> {
        for _ in 0..8 {
            let mut seed = [0u8; 32];
            getrandom::getrandom(&mut seed).map_err(|error| format!("随机数不可用：{error}"))?;
            let Ok(signing) = SigningKey::from_slice(&seed) else {
                continue;
            };
            let public = public_jwk_of(&signing);
            let mut private = public.clone();
            if let Some(object) = private.as_object_mut() {
                object.insert(
                    "d".to_string(),
                    Value::String(URL_SAFE_NO_PAD.encode(seed)),
                );
            }
            return Ok(json!({
                "private_key_jwk": private,
                "public_key_jwk": public,
            }));
        }
        Err("无法生成本机 P-256 密钥，请重试".to_string())
    }

    /// 现签一份 proof。
    ///
    /// `method` 与 `endpoint` 必须与即将发出的请求**逐字**一致（上游会比对，
    /// 对不上就是 401）；`htu` 用完整地址，不带 fragment。
    pub fn proof(&self, method: &str, endpoint: &str, now_ms: i64) -> Result<String, String> {
        let header = json!({ "alg": ALG, "typ": TYP, "jwk": self.public_jwk });
        let header_json =
            serde_json::to_vec(&header).map_err(|error| format!("DPoP 头部序列化失败：{error}"))?;
        // jti 用 32 字节随机数的十六进制（与参考实现的 randomHex(32) 同形状）
        let mut jti = [0u8; 32];
        getrandom::getrandom(&mut jti).map_err(|error| format!("随机数不可用：{error}"))?;
        let payload = json!({
            "htm": method.to_ascii_uppercase(),
            "htu": endpoint,
            "iat": now_ms / 1000,
            "jti": jti.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
        });
        let payload_json = serde_json::to_vec(&payload)
            .map_err(|error| format!("DPoP 载荷序列化失败：{error}"))?;
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&header_json),
            URL_SAFE_NO_PAD.encode(&payload_json)
        );
        let digest = Sha256::digest(signing_input.as_bytes());
        let signature: Signature = self
            .signing
            .sign_prehash(&digest)
            .map_err(|error| format!("DPoP 签名失败：{error}"))?;
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        ))
    }
}

/// 由私钥推出公钥 JWK；`x` / `y` 必须是 32 字节定长（大整数去掉前导零后要补齐，
/// 不补就会短一截、被上游判成非法点）。
fn public_jwk_of(signing: &SigningKey) -> Value {
    let verifying: &VerifyingKey = signing.verifying_key();
    let point = verifying.to_encoded_point(false);
    let x = point.x().map(|bytes| bytes.to_vec()).unwrap_or_default();
    let y = point.y().map(|bytes| bytes.to_vec()).unwrap_or_default();
    json!({
        "kty": "EC",
        "crv": "P-256",
        "x": URL_SAFE_NO_PAD.encode(x),
        "y": URL_SAFE_NO_PAD.encode(y),
    })
}

fn text_of(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}
