//! 国际 COSY 完整客户端身份组沿用原生千问基线（非国际实测），与 body 的 qoder_work/processing 一致。
//! AES-CBC/PKCS7 与 RSA PKCS#1 v1.5 使用维护中的 RustCrypto；随机由宿主种子提供。
//! AES IV 等于 key、1024 位 RSA、MD5 是既有线协议，不是本项目新密码设计。
//! 这里只用普通机器编号；Cosy-Clientip = machineId，绝不套 Qoder 的回环地址。
use super::{credentials::Credentials, endpoints, util};
use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use base64::Engine;
use md5::{Digest, Md5};
use rand_chacha::{rand_core::SeedableRng, ChaCha20Rng};
use rsa::{BigUint, Pkcs1v15Encrypt, RsaPublicKey};

const RSA_MODULUS: &str = "c0f22307e5cd362e296bb04470f6de8fbf935ce24e8fcf511a0e2701329769c4a76e499bb938036a52af1eaf818cf79a2600620e3ce87e371d2ca6d85803606a1b3fa5e874643c9ed2db7e85673ef7227fca56e2e7c08f0927609bb896a9f24be1782099a66016a5bfdc3f1ff756bfc9e88d7b5dc5be30bf45a0223a00ebcecf";
const CUSTOM: &[u8] = b"_doRTgHZBKcGVjlvpC,@aFSx#DPuNJme&i*MzLOEn)sUrthbf%Y^w.(kIQyXqWA!";
const STANDARD: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn machine_os() -> Result<String> {
    let platform = sdk::host("runtime.platform", json!({}))?;
    let arch = sdk::host("runtime.architecture", json!({}))?;
    let platform = match platform.as_str() {
        Some("win") => "windows",
        Some("mac") => "darwin",
        Some("linux") => "linux",
        _ => return Err(Error::http(503, "千问办公 COSY 缺少受支持的主机系统，未猜测设备信息")),
    };
    let arch = match arch.as_str() {
        Some("x86_64") => "x86_64",
        Some("aarch64") => "aarch64",
        _ => {
            return Err(Error::http(
                503,
                "千问办公 COSY 缺少受支持的主机架构，未用 WASM 目标代替",
            ))
        }
    };
    Ok(format!("{arch}_{platform}"))
}
pub fn headers(body: Option<&[u8]>, path: &str, credentials: &Credentials) -> Result<Value> {
    if credentials.user.is_empty() || credentials.machine.is_empty() || credentials.access.is_empty() {
        return Err(Error::http(400, "千问办公 COSY 缺少账号或设备编号，请重新导入完整记录"));
    }
    credentials.validate_public()?;
    let os = machine_os()?;
    let secret = util::random_secret()?;
    let key = secret
        .get(..16)
        .ok_or_else(|| Error::new("千问办公签名随机密钥长度无效"))?;
    let text = |value: &str| serde_json::to_string(value).map_err(|_| Error::new("千问办公签名信息不能序列化"));
    let info = format!(
        "{{\"uid\":{},\"security_oauth_token\":{},\"name\":{},\"aid\":\"\",\"email\":{}}}",
        text(&credentials.user)?,
        text(&credentials.access)?,
        text(&credentials.name)?,
        text(&credentials.email)?
    );
    let encrypted = cbc::Encryptor::<aes::Aes128>::new_from_slices(key.as_bytes(), key.as_bytes())
        .map_err(|_| Error::new("千问办公 AES 初始化失败"))?
        .encrypt_padded_vec_mut::<Pkcs7>(info.as_bytes());
    let info = base64::engine::general_purpose::STANDARD.encode(encrypted);
    let modulus = BigUint::parse_bytes(RSA_MODULUS.as_bytes(), 16).ok_or_else(|| Error::new("千问办公内置公钥无效"))?;
    let public_key =
        RsaPublicKey::new(modulus, BigUint::from(65537_u32)).map_err(|_| Error::new("千问办公公钥初始化失败"))?;
    let mut rng = ChaCha20Rng::from_seed(util::random::<32>()?);
    let encrypted_key = public_key
        .encrypt(&mut rng, Pkcs1v15Encrypt, key.as_bytes())
        .map_err(|_| Error::new("千问办公签名密钥加密失败"))?;
    let cosy_key = base64::engine::general_purpose::STANDARD.encode(encrypted_key);
    let request = util::uuid()?;
    let timestamp = (util::now()? / 1000).to_string();
    let payload = format!("{{\"version\":\"v1\",\"requestId\":\"{request}\",\"info\":\"{info}\",\"cosyVersion\":\"{}\",\"ideVersion\":\"\"}}", endpoints::COSY_VERSION);
    let payload = base64::engine::general_purpose::STANDARD.encode(payload.as_bytes());
    let body = body.unwrap_or(&[]);
    let sig_path = path
        .split('?')
        .next()
        .unwrap_or(path)
        .strip_prefix("/algo")
        .ok_or_else(|| Error::new("千问办公签名路径不是 algo 接口"))?;
    if !sig_path.starts_with('/') {
        return Err(Error::new("千问办公签名路径无效"));
    }
    let mut hasher = Md5::new();
    for part in [payload.as_bytes(), cosy_key.as_bytes(), timestamp.as_bytes(), body] {
        hasher.update(part);
        hasher.update(b"\n");
    }
    hasher.update(sig_path.as_bytes());
    let signature = format!("{:x}", hasher.finalize());
    // 原生 MachineType 仍为 5，不要把客户端 ClientType=6 错写到机器类型。
    Ok(
        json!({ "Authorization": format!("Bearer COSY.{payload}.{signature}"), "Cosy-Key": cosy_key,
        "Cosy-User": credentials.user, "Cosy-Date": timestamp, "Cosy-Version": endpoints::COSY_VERSION,
        "Cosy-Machineid": credentials.machine, "Cosy-Machinetoken": credentials.machine, "Cosy-Machinetype": "5",
        "Cosy-Machineos": os, "Cosy-Clienttype": endpoints::CLIENT_TYPE, "Cosy-Clientip": credentials.machine,
        "Cosy-Bodyhash": format!("{:x}", Md5::digest(body)), "Cosy-Bodylength": body.len().to_string(), "Cosy-Sigpath": sig_path,
        "Cosy-Data-Policy": endpoints::DATA_POLICY, "Cosy-Organization-Id": "", "Cosy-Organization-Tags": "",
        "Cosy-Scene": endpoints::SCENE, "Cosy-Business-Product": endpoints::PRODUCT, "Cosy-Business-Type": endpoints::BUSINESS_TYPE,
        "Login-Version": "v2", "X-Request-Id": request }),
    )
}
/// Encode=1 是标准 base64 → 尾/中/首段重排 → 字母表替换，不是压缩。
pub fn encode(payload: &[u8]) -> Vec<u8> {
    let standard = base64::engine::general_purpose::STANDARD.encode(payload);
    let bytes = standard.as_bytes();
    let third = bytes.len() / 3;
    let mut table = [0_u8; 256];
    for (index, slot) in table.iter_mut().enumerate() {
        *slot = index as u8;
    }
    for (index, byte) in STANDARD.iter().enumerate() {
        table[*byte as usize] = CUSTOM[index];
    }
    table[b'=' as usize] = b'$';
    [bytes.len() - third..bytes.len(), third..bytes.len() - third, 0..third]
        .into_iter()
        .flat_map(|range| bytes[range].iter().map(|byte| table[*byte as usize]))
        .collect()
}
