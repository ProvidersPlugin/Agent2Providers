//! 本仓 codearts/dpop.rs 的纯 Rust 密钥校验与 proof 核心（2026-11-10）。
//! 本批不导出登录/续期调用；proof 不交给辅助程序，私钥从不离开受控账号数据。

use agent2api_wasm_sdk::Result;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use p256::ecdsa::{signature::hazmat::PrehashSigner, Signature, SigningKey};
use sha2::{Digest, Sha256};

use crate::{
    credentials::{DpopPair, Jwk},
    errors, util,
};

fn same_coord(declared: &str, expected: &str) -> bool {
    let normalized = URL_SAFE_NO_PAD
        .decode(declared.trim().trim_end_matches('='))
        .map(|bytes| URL_SAFE_NO_PAD.encode(bytes));
    matches!(normalized, Ok(value) if value == expected)
}

fn key(pair: &DpopPair) -> Result<(SigningKey, Jwk)> {
    let private = &pair.private_key_jwk;
    if private.kty.trim() != "EC" || private.crv.trim() != "P-256" {
        return Err(errors::invalid("CodeArts DPoP 材料必须是 EC / P-256"));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(private.d.trim().trim_end_matches('='))
        .map_err(|_| errors::invalid("CodeArts DPoP 私钥编码无效"))?;
    if bytes.len() != 32 {
        return Err(errors::invalid("CodeArts DPoP 私钥长度无效"));
    }
    if !pair.public_key_jwk.d.trim().is_empty() {
        return Err(errors::invalid("CodeArts DPoP 公钥对象不能包含私钥材料"));
    }
    let key = SigningKey::from_slice(&bytes)
        .map_err(|_| errors::invalid("CodeArts DPoP 私钥不是有效标量"))?;
    let point = key.verifying_key().to_encoded_point(false);
    let (Some(x), Some(y)) = (point.x(), point.y()) else {
        return Err(errors::invalid("CodeArts DPoP 公钥无效"));
    };
    let public = Jwk {
        kty: "EC".into(),
        crv: "P-256".into(),
        x: URL_SAFE_NO_PAD.encode(x),
        y: URL_SAFE_NO_PAD.encode(y),
        d: String::new(),
    };
    if (!private.x.trim().is_empty() && !same_coord(&private.x, &public.x))
        || (!private.y.trim().is_empty() && !same_coord(&private.y, &public.y))
    {
        return Err(errors::invalid("CodeArts DPoP 公私钥资料不一致"));
    }
    let declared = &pair.public_key_jwk;
    let has_public = [
        declared.kty.as_str(),
        declared.crv.as_str(),
        declared.x.as_str(),
        declared.y.as_str(),
    ]
    .into_iter()
    .any(|value| !value.trim().is_empty());
    if has_public
        && (declared.kty.trim() != "EC"
            || declared.crv.trim() != "P-256"
            || !same_coord(&declared.x, &public.x)
            || !same_coord(&declared.y, &public.y))
    {
        return Err(errors::invalid("CodeArts DPoP 公私钥资料不一致"));
    }
    Ok((key, public))
}

pub fn validate(pair: &DpopPair) -> Result<()> {
    key(pair).map(|_| ())
}

// 仅保留本家固定令牌端点的签名原语，不注册网络或管理入口；续期仍是明确门禁。
#[allow(dead_code)]
pub fn token_proof(pair: &DpopPair) -> Result<String> {
    let (key, public) = key(pair)?;
    let header = agent2api_wasm_sdk::json!({ "alg": "ES256", "typ": "dpop+jwt", "jwk": public });
    let payload = agent2api_wasm_sdk::json!({ "htm": "POST",
        "htu": "https://sts.cn-north-4.myhuaweicloud.com/v1/oauth2/tokens",
        "iat": util::now()? / 1000, "jti": util::hex(&util::random::<32>()?) });
    let input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(payload.to_string())
    );
    let signature: Signature = key
        .sign_prehash(&Sha256::digest(input.as_bytes()))
        .map_err(|_| errors::protocol("CodeArts DPoP 签名失败"))?;
    Ok(format!(
        "{input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}
