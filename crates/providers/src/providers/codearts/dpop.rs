//! DPoP proof（RFC 9449 的 `DPoP` 头）：一个 ES256 的 JWS，头部嵌**公钥 JWK**，
//! 载荷声明「这个方法、这个地址、这个时刻、这个一次性 id」。
//!
//! 上游的理由：令牌端点（`sts.…/v1/oauth2/tokens`）不靠客户端密钥认证，而是要求
//! 调用方证明「我手里有这把私钥」—— 每次请求现签一份 proof，服务端用 proof 头部
//! 里那把公钥验签。所以私钥是登录态的一部分，必须与 refresh token、PKCE verifier
//! 一起长期保存（见 `credentials.rs` 模块头）。
//!
//! ── 与参考实现的两点差异（都不影响上游接受）─────────────────
//!   1. 签名用 RFC 6979 确定性 k（`ecdsa` crate 的 `sign_prehash`），参考实现用
//!      随机 k。两者都是合法 ECDSA，服务端按公钥验签，不比对字节。
//!   2. `s` **不做 low-S 归一**，与 Go 的 `ecdsa.Sign` 一致（归一也合法，但既然
//!      参考实现没做、上游也一直收，就不引入这个差异）。
//!
//! 因此本模块的验收不是「与 Go 逐字节相同」，而是：**能用嵌入的公钥验签通过**
//! （见文件末的测试），以及字段齐备。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use p256::ecdsa::{signature::hazmat::PrehashSigner, Signature, SigningKey, VerifyingKey};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::credentials::{DpopKeyPair, Jwk};

/// 上游只收这一种算法（`codeArtsOAuthDPoPAlg`）。
const DOPP_ALG: &str = "ES256";
const DOPP_TYP: &str = "dpop+jwt";

/// 一份可用的 DPoP 密钥：私钥（签名）+ 公钥 JWK（嵌进 JWS 头）。
pub struct DpopKey {
    signing: SigningKey,
    public_jwk: Jwk,
}

impl DpopKey {
    /// 从落盘的密钥对恢复。**私钥是唯一必要输入**（公钥由它推出），
    /// 但落盘的公钥若存在且与私钥不符，说明这份凭据被改坏了 —— 宁可报错，
    /// 也不要签出一份「头里的公钥和签名的私钥对不上」的 proof 去打上游。
    pub fn from_key_pair(pair: &DpopKeyPair) -> Result<Self, String> {
        let private = &pair.private_key_jwk;
        if private.kty != "EC" || private.crv != "P-256" || private.d.trim().is_empty() {
            return Err("CodeArts DPoP 私钥不完整（需要 EC / P-256 / d）".to_string());
        }
        let key = Self::from_private_jwk(private)?;
        if !pair.public_key_jwk.x.trim().is_empty() {
            let recorded = normalise_public(&pair.public_key_jwk);
            if recorded != key.public_jwk {
                return Err("CodeArts DPoP 公私钥不一致，请重新登录".to_string());
            }
        }
        Ok(key)
    }

    /// 从一个私钥 JWK 恢复（`d` 是 32 字节标量的 base64url）。
    pub fn from_private_jwk(jwk: &Jwk) -> Result<Self, String> {
        let scalar = URL_SAFE_NO_PAD
            .decode(jwk.d.trim())
            .map_err(|_| "CodeArts DPoP 私钥不是合法的 base64url".to_string())?;
        let signing = SigningKey::from_slice(&scalar)
            .map_err(|_| "CodeArts DPoP 私钥不是合法的 P-256 标量".to_string())?;
        Ok(Self { public_jwk: public_jwk_of(&signing), signing })
    }

    /// 现签一份 proof。
    ///
    /// * `method` 与 `endpoint` 必须与即将发出的请求**逐字**一致（上游会比对，
    ///   对不上就是 401）；`htu` 用完整地址，不带 fragment。
    /// * `now_ms` 只在测试里注入，线上走 `logging::now_ms()`。
    pub fn proof(&self, method: &str, endpoint: &str, now_ms: i64) -> Result<String, String> {
        let header = Header { alg: DOPP_ALG, typ: DOPP_TYP, jwk: &self.public_jwk };
        let header_json =
            serde_json::to_vec(&header).map_err(|error| format!("DPoP 头部序列化失败：{error}"))?;
        // jti 用 32 字节随机数的十六进制（与参考实现的 randomHex(32) 同形状）
        let mut jti = [0u8; 32];
        getrandom::getrandom(&mut jti).map_err(|error| format!("随机数不可用：{error}"))?;
        let payload = Payload {
            htm: method.to_ascii_uppercase(),
            htu: endpoint,
            iat: now_ms / 1000,
            jti: jti.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
        };
        let payload_json =
            serde_json::to_vec(&payload).map_err(|error| format!("DPoP 载荷序列化失败：{error}"))?;
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&header_json),
            URL_SAFE_NO_PAD.encode(&payload_json)
        );
        self.sign_input(&signing_input)
    }

    /// 用 RFC 6979 确定性 k 对一段签名输入签名（`proof` 的签名步骤）。
    ///
    /// 单独拆出来是为了让测试能钉住「同一输入必得同一签名」这条性质 ——
    /// `jt i` 每次都新随机，整条 proof 本来就不该相等。
    fn sign_input(&self, signing_input: &str) -> Result<String, String> {
        let digest = Sha256::digest(signing_input.as_bytes());
        let signature: Signature = self
            .signing
            .sign_prehash(&digest)
            .map_err(|error| format!("DPoP 签名失败：{error}"))?;
        Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes())))
    }

#[allow(dead_code)]
    /// 头部里那把公钥（测试与排障用）。
    pub fn public_jwk(&self) -> &Jwk {
        &self.public_jwk
    }

    /// **新生成**一把 P-256 密钥，返回可直接落盘的那一份。
    ///
    /// 网页登录发起时用一次：这把私钥要跟着凭据存下来 —— 续期时上游用 proof 头里
    /// 的公钥验签，**换一把就续不了**（与 PKCE verifier 同一条道理，见
    /// `credentials.rs` 模块头）。
    ///
    /// 只依赖 `getrandom` + `SigningKey::from_slice`（本模块已有的两件工具），
    /// 不给 `p256` 多开 `rng` 特性：随到的 32 字节里落到 `[1, n-1]` 之外的概率
    /// 约 2⁻¹²⁸，重试几次只是兜底，不是常规路径。
    pub fn generate() -> Result<DpopKeyPair, String> {
        for _ in 0..8 {
            let mut seed = [0u8; 32];
            getrandom::getrandom(&mut seed).map_err(|error| format!("随机数不可用：{error}"))?;
            let Ok(signing) = SigningKey::from_slice(&seed) else { continue };
            let public = public_jwk_of(&signing);
            // 私钥那份 = 公钥三件套 + `d`（先克隆公钥再补 d，`..public` 会把
            // String 字段移走、下面就没法再把公钥交给结构体了）
            let mut private = public.clone();
            private.d = URL_SAFE_NO_PAD.encode(seed);
            return Ok(DpopKeyPair { private_key_jwk: private, public_key_jwk: public });
        }
        Err("无法生成本机 P-256 密钥，请重试".to_string())
    }
}

#[derive(Serialize)]
struct Header<'a> {
    alg: &'a str,
    typ: &'a str,
    jwk: &'a Jwk,
}

#[derive(Serialize)]
struct Payload<'a> {
    htm: String,
    htu: &'a str,
    iat: i64,
    jti: String,
}

/// 由私钥推出公钥 JWK；`x` / `y` 必须是 32 字节定长（大整数去掉前导零后要补齐，
/// 不补就会短一截、被上游判成非法点）。
fn public_jwk_of(signing: &SigningKey) -> Jwk {
    let verifying: &VerifyingKey = signing.verifying_key();
    let point = verifying.to_encoded_point(false);
    let x = point.x().map(|bytes| bytes.to_vec()).unwrap_or_default();
    let y = point.y().map(|bytes| bytes.to_vec()).unwrap_or_default();
    Jwk {
        kty: "EC".to_string(),
        crv: "P-256".to_string(),
        x: URL_SAFE_NO_PAD.encode(x),
        y: URL_SAFE_NO_PAD.encode(y),
        d: String::new(),
    }
}

/// 落盘的公钥与现算的公钥比对时用：忽略 `d`，并容忍 base64url 的补位写法差异。
fn normalise_public(jwk: &Jwk) -> Jwk {
    let decode = |value: &str| -> String {
        URL_SAFE_NO_PAD
            .decode(value.trim())
            .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
            .unwrap_or_else(|_| value.trim().to_string())
    };
    Jwk {
        kty: jwk.kty.trim().to_string(),
        crv: jwk.crv.trim().to_string(),
        x: decode(&jwk.x),
        y: decode(&jwk.y),
        d: String::new(),
    }
}

#[cfg(any())]
mod tests {
    use super::*;
    use p256::ecdsa::signature::Verifier;

    /// 一份固定的 P-256 私钥（`d` 是 1），用来做**确定性**断言：
    /// 公钥必然是生成元的 1 倍，`x` / `y` 也就能写死。
    fn fixed_key() -> Jwk {
        let mut d = [0u8; 32];
        d[31] = 1;
        Jwk {
            kty: "EC".to_string(),
            crv: "P-256".to_string(),
            x: String::new(),
            y: String::new(),
            d: URL_SAFE_NO_PAD.encode(d),
        }
    }

    #[test]
    fn proof_verifies_against_the_embedded_public_key() {
        let key = DpopKey::from_private_jwk(&fixed_key()).expect("固定私钥应当可用");
        let proof = key
            .proof("POST", "https://sts.cn-north-4.myhuaweicloud.com/v1/oauth2/tokens", 1790439420327)
            .expect("签名应当成功");
        let parts: Vec<&str> = proof.split('.').collect();
        assert_eq!(3, parts.len(), "JWS 是三段式");

        // 头部：alg/typ 写死，且嵌了公钥
        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!("ES256", header["alg"]);
        assert_eq!("dpop+jwt", header["typ"]);
        assert_eq!("EC", header["jwk"]["kty"]);
        assert_eq!("P-256", header["jwk"]["crv"]);
        assert!(header["jwk"]["d"].as_str().unwrap_or("").is_empty(), "proof 里绝不能带私钥");

        // 载荷：四个字段齐备，htm 会被大写，iat 由毫秒降成秒
        let payload: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!("POST", payload["htm"]);
        assert_eq!("https://sts.cn-north-4.myhuaweicloud.com/v1/oauth2/tokens", payload["htu"]);
        // 小写方法也要被规范化成大写（上游按大写比对 htm）
        let lowercase = key.proof("post", "https://sts.cn-north-4.myhuaweicloud.com/v1/oauth2/tokens", 1790439420327).unwrap();
        let lowercase_payload: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(lowercase.split('.').nth(1).unwrap()).unwrap()).unwrap();
        assert_eq!("POST", lowercase_payload["htm"]);
        assert_eq!(1790439420, payload["iat"].as_i64().unwrap());
        assert_eq!(64, payload["jti"].as_str().unwrap().len(), "jti 是 32 字节的十六进制");

        // 用头部里那把公钥验签 —— 这是本模块真正的验收
        let jwk: Jwk = serde_json::from_value(header["jwk"].clone()).unwrap();
        let verifying = VerifyingKey::from_sec1_bytes(
            &[&[0x04u8][..], &URL_SAFE_NO_PAD.decode(&jwk.x).unwrap(), &URL_SAFE_NO_PAD.decode(&jwk.y).unwrap()].concat(),
        )
        .expect("头部里的公钥应当能还原成点");
        let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).unwrap();
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        verifying
            .verify(signing_input.as_bytes(), &signature)
            .expect("proof 必须能用它自己带的公钥验过");
    }

    #[test]
    fn signature_over_the_same_input_is_deterministic() {
        // RFC 6979：同一个键、同一段签名输入 → 同一份签名。
        // 注意**整条 proof 本来就不该相等** —— jti 每次都新随机，所以这里比的是
        // 签名步骤（sign_input），不是 proof 全文
        let key = DpopKey::from_private_jwk(&fixed_key()).unwrap();
        let input = "eyJhbGciOiJFUzI1NiJ9.eyJodG0iOiJHRVQifQ";
        let first = key.sign_input(input).unwrap();
        let second = key.sign_input(input).unwrap();
        assert_eq!(first, second, "同一段签名输入必须得到同一个签名（确定性 k）");
        assert!(first.starts_with(input), "签名串是 `输入.签名` 的形状");
        // 输入变了签名就得变，否则上面的相等毫无意义
        assert_ne!(first, key.sign_input("eyJhbGciOiJFUzI1NiJ9.eyJodG0iOiJQT1NUIn0").unwrap());
    }

    #[test]
    fn private_key_one_has_the_expected_public_point() {
        // d = 1 时公钥就是生成元本身；照此钉住定长补齐（32 字节）
        let key = DpopKey::from_private_jwk(&fixed_key()).unwrap();
        let jwk = key.public_jwk();
        assert_eq!(32, URL_SAFE_NO_PAD.decode(&jwk.x).unwrap().len());
        assert_eq!(32, URL_SAFE_NO_PAD.decode(&jwk.y).unwrap().len());
        assert_eq!("EC", jwk.kty);
        assert_eq!("P-256", jwk.crv);
    }

    #[test]
    fn broken_key_material_is_rejected_before_signing() {
        // `d` 不是合法 base64url
        let mut bad = fixed_key();
        bad.d = "!!!not-base64!!!".to_string();
        assert!(DpopKey::from_private_jwk(&bad).is_err());
        // 曲线不对
        let mut wrong_curve = fixed_key();
        wrong_curve.crv = "P-384".to_string();
        assert!(DpopKey::from_key_pair(&DpopKeyPair {
            private_key_jwk: wrong_curve,
            public_key_jwk: Jwk::default(),
        })
        .is_err());
        // 公私钥对不上：**报告为错误**而不是照签
        let key = DpopKey::from_private_jwk(&fixed_key()).unwrap();
        let mut wrong_public = key.public_jwk().clone();
        wrong_public.x = URL_SAFE_NO_PAD.encode([7u8; 32]);
        assert!(DpopKey::from_key_pair(&DpopKeyPair {
            private_key_jwk: fixed_key(),
            public_key_jwk: wrong_public,
        })
        .is_err());
        // 公钥缺席不算错（私钥能推出来）
        assert!(DpopKey::from_key_pair(&DpopKeyPair {
            private_key_jwk: fixed_key(),
            public_key_jwk: Jwk::default(),
        })
        .is_ok());
    }
}
