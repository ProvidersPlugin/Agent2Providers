//! COSY 中国版签名与 Encode=1 编码，按现有原生字节协议移植。
//! AES-CBC/PKCS7 与 RSA PKCS#1 v1.5 使用 RustCrypto，不手写密码或启动系统进程。
//! AES IV 等于密钥、RSA 1024 位和 MD5 是上游既定线协议，不是本项目新设计。
//! RSA 用每请求宿主随机种子驱动 ChaCha20Rng；没有 getrandom、线程或环境随机源。
//! 原生只用 base64 自定义编码而没有压缩，不引入 flate2（原依赖用于国际 runtime-info）。
use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use base64::Engine;
use md5::{Digest, Md5};
use rand_chacha::{rand_core::SeedableRng, ChaCha20Rng};
use rsa::{BigUint, Pkcs1v15Encrypt, RsaPublicKey};
use super::{credentials::Credentials, endpoints, util};

const RSA_MODULUS: &str = "c0f22307e5cd362e296bb04470f6de8fbf935ce24e8fcf511a0e2701329769c4a76e499bb938036a52af1eaf818cf79a2600620e3ce87e371d2ca6d85803606a1b3fa5e874643c9ed2db7e85673ef7227fca56e2e7c08f0927609bb896a9f24be1782099a66016a5bfdc3f1ff756bfc9e88d7b5dc5be30bf45a0223a00ebcecf";
const CUSTOM: &[u8] = b"_doRTgHZBKcGVjlvpC,@aFSx#DPuNJme&i*MzLOEn)sUrthbf%Y^w.(kIQyXqWA!";
const STANDARD: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn machine_os() -> Result<String> {
    let platform = sdk::host("runtime.platform", json!({}))?;
    let arch = sdk::host("runtime.architecture", json!({}))?;
    let platform = match platform.as_str() {
        Some("win") => "windows", Some("mac") => "darwin", Some("linux") => "linux",
        _ => return Err(Error::http(503, "Qoder COSY 暂不支持当前主机系统，未猜测设备信息")),
    };
    let arch = match arch.as_str() {
        Some("x86_64") => "x86_64", Some("aarch64") => "aarch64",
        _ => return Err(Error::http(503, "Qoder COSY 缺少受支持的主机架构，未使用 WASM 架构代替")),
    };
    Ok(format!("{arch}_{platform}"))
}
pub fn headers(body: Option<&[u8]>, path: &str, credentials: &Credentials) -> Result<Value> {
    headers_for_client(body, path, credentials, endpoints::CLI_CLIENT_TYPE)
}
pub fn headers_for_client(
    body: Option<&[u8]>,
    path: &str,
    credentials: &Credentials,
    client_type: &str,
) -> Result<Value> {
    if credentials.user.is_empty() || credentials.machine.is_empty() || credentials.access.is_empty() {
        return Err(Error::http(400, "Qoder COSY 缺少账号或机器标识，请重新导入"));
    }
    util::identity(&credentials.user)?; util::identity(&credentials.machine)?;
    let os = machine_os()?;
    let secret = util::random_secret()?;
    let key = secret.get(..16).ok_or_else(|| Error::new("Qoder 签名随机密钥长度无效"))?;
    let text = |value: &str| serde_json::to_string(value).map_err(|_| Error::new("Qoder 签名信息不能序列化"));
    // 与原生一样固定 JSON 键顺序，不依赖 serde_json 的 Map 排序。
    let info = format!("{{\"uid\":{},\"security_oauth_token\":{},\"name\":{},\"aid\":\"\",\"email\":{}}}",
        text(&credentials.user)?, text(&credentials.access)?, text(&credentials.name)?, text(&credentials.email)?);
    let encrypted = cbc::Encryptor::<aes::Aes128>::new_from_slices(key.as_bytes(), key.as_bytes())
        .map_err(|_| Error::new("Qoder AES 初始化失败"))?.encrypt_padded_vec_mut::<Pkcs7>(info.as_bytes());
    let info = base64::engine::general_purpose::STANDARD.encode(encrypted);
    let modulus = BigUint::parse_bytes(RSA_MODULUS.as_bytes(), 16).ok_or_else(|| Error::new("Qoder 内置公钥无效"))?;
    let public_key = RsaPublicKey::new(modulus, BigUint::from(65537_u32)).map_err(|_| Error::new("Qoder 公钥初始化失败"))?;
    let mut rng = ChaCha20Rng::from_seed(util::random::<32>()?);
    let encrypted_key = public_key.encrypt(&mut rng, Pkcs1v15Encrypt, key.as_bytes()).map_err(|_| Error::new("Qoder 签名密钥加密失败"))?;
    let cosy_key = base64::engine::general_purpose::STANDARD.encode(encrypted_key);
    let request = util::uuid()?;
    let timestamp = (util::now()? / 1000).to_string();
    let payload = format!("{{\"version\":\"v1\",\"requestId\":\"{request}\",\"info\":\"{info}\",\"cosyVersion\":\"{}\",\"ideVersion\":\"\"}}", endpoints::COSY_VERSION);
    let payload = base64::engine::general_purpose::STANDARD.encode(payload.as_bytes());
    let body = body.unwrap_or(&[]);
    let sig_path = path.split('?').next().unwrap_or(path).strip_prefix("/algo").ok_or_else(|| Error::new("Qoder 签名路径不是 algo 接口"))?;
    if !sig_path.starts_with('/') { return Err(Error::new("Qoder 签名路径无效")); }
    let mut hasher = Md5::new();
    for part in [payload.as_bytes(), cosy_key.as_bytes(), timestamp.as_bytes(), body] { hasher.update(part); hasher.update(b"\n"); }
    hasher.update(sig_path.as_bytes());
    let signature = format!("{:x}", hasher.finalize());
    Ok(json!({ "Authorization": format!("Bearer COSY.{payload}.{signature}"), "Cosy-Key": cosy_key,
        "Cosy-User": credentials.user, "Cosy-Date": timestamp, "Cosy-Version": endpoints::COSY_VERSION,
        "Cosy-Machineid": credentials.machine, "Cosy-Machinetoken": credentials.machine,
        "Cosy-Machinetype": "5", "Cosy-Machineos": os, "Cosy-Clienttype": client_type, "Cosy-Clientip": "127.0.0.1",
        "Cosy-Bodyhash": format!("{:x}", Md5::digest(body)), "Cosy-Bodylength": body.len().to_string(), "Cosy-Sigpath": sig_path,
        "Cosy-Data-Policy": "disagree", "Cosy-Organization-Id": "", "Cosy-Organization-Tags": "",
        "Login-Version": "v2", "X-Request-Id": request }))
}

/// 原生编码没有 gzip/压缩：标准 base64 → 尾/中/首三段重排 → 自定义字母表。
pub fn encode(payload: &[u8]) -> Vec<u8> {
    let standard = base64::engine::general_purpose::STANDARD.encode(payload);
    let bytes = standard.as_bytes();
    let third = bytes.len() / 3;
    let mut table = [0_u8; 256];
    for (index, slot) in table.iter_mut().enumerate() { *slot = index as u8; }
    for (index, byte) in STANDARD.iter().enumerate() { table[*byte as usize] = CUSTOM[index]; }
    table[b'=' as usize] = b'$';
    [bytes.len() - third..bytes.len(), third..bytes.len() - third, 0..third].into_iter()
        .flat_map(|range| bytes[range].iter().map(|byte| table[*byte as usize])).collect()
}
