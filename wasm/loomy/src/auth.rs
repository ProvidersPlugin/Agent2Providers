//! 固定账号服务的只读身份查询与密码登录；密码只属于本次明确输入，不保存或自动重登。
use agent2api_wasm_sdk::{json, Error, Result, Value};
use base64::Engine;
use rand_core::{CryptoRng, RngCore};
use rsa::{pkcs8::DecodePublicKey, traits::PublicKeyParts, Pkcs1v15Encrypt, RsaPublicKey};

use super::{account, client, errors, sign};

pub fn base() -> Result<Value> {
    Ok(
        json!({ "appid": "GM3LOOMY", "modelid": "Web", "version": "1.0.0",
        "devid": "web", "ua": "Loomy|Desktop|Electron|macOS", "traceid": sign::random_hex(16)? }),
    )
}

pub fn data(payload: &Value) -> Result<&Value> {
    payload
        .pointer("/data/data")
        .filter(|value| value.is_object())
        .or_else(|| payload.get("data").filter(|value| value.is_object()))
        .ok_or_else(|| Error::http(502, "Loomy 账号接口未返回有效数据"))
}

pub fn user_info(session: &str) -> Result<Value> {
    let body = json!({ "base": base()?, "param": { "session": session } });
    let payload = client::signed_post(
        "/userinfo/query/baseInfo",
        &body,
        "账号信息查询",
        &[session],
    )?;
    errors::ensure_success(&payload, "账号信息查询", session)?;
    Ok(data(&payload)?.clone())
}

/// RSA 填充只从宿主随机服务取字节，失败必须停止，不使用系统随机或弱兜底。
struct HostRandom;
impl CryptoRng for HostRandom {}
impl RngCore for HostRandom {
    fn next_u32(&mut self) -> u32 {
        let mut bytes = [0; 4];
        self.fill_bytes(&mut bytes);
        u32::from_le_bytes(bytes)
    }
    fn next_u64(&mut self) -> u64 {
        let mut bytes = [0; 8];
        self.fill_bytes(&mut bytes);
        u64::from_le_bytes(bytes)
    }
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        // rsa 0.9 的填充使用不可失败接口；宿主失败直接陷阱，绝不生成弱随机或提交登录。
        self.try_fill_bytes(bytes)
            .expect("Loomy host randomness unavailable");
    }
    fn try_fill_bytes(&mut self, bytes: &mut [u8]) -> std::result::Result<(), rand_core::Error> {
        for chunk in bytes.chunks_mut(256) {
            let result = agent2api_wasm_sdk::host("random.bytes", json!({ "length": chunk.len() }))
                .map_err(|_| {
                    rand_core::Error::from(
                        std::num::NonZeroU32::new(rand_core::Error::CUSTOM_START).unwrap(),
                    )
                })?;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(result.as_str().unwrap_or(""))
                .map_err(|_| {
                    rand_core::Error::from(
                        std::num::NonZeroU32::new(rand_core::Error::CUSTOM_START).unwrap(),
                    )
                })?;
            if decoded.len() != chunk.len() {
                return Err(std::num::NonZeroU32::new(rand_core::Error::CUSTOM_START)
                    .unwrap()
                    .into());
            }
            chunk.copy_from_slice(&decoded);
        }
        Ok(())
    }
}

pub fn password(inputs: &Value, name: Option<&str>) -> Result<Value> {
    let login = inputs["loginId"].as_str().unwrap_or("").trim();
    let password = inputs["password"].as_str().unwrap_or("");
    if login.is_empty()
        || login.len() > 256
        || login.chars().any(char::is_control)
        || password.is_empty()
        || password.len() > 1024
        || password.contains('\0')
    {
        return Err(Error::http(400, "请填写有效的 Loomy 账号与密码"));
    }
    let payload = client::signed_post(
        "/login/account/getPuKey",
        &json!({ "base": base()? }),
        "登录公钥查询",
        &[login, password],
    )?;
    errors::ensure_success(&payload, "登录公钥查询", password)?;
    let data = data(&payload)?;
    let public = account::text(data, &["pukey"]);
    let rcode = account::text(data, &["rcode"]);
    if public.len() > 16 * 1024 || rcode.is_empty() || rcode.len() > 8192 {
        return Err(Error::http(502, "Loomy 登录公钥或随机验证值无效"));
    }
    let der = base64::engine::general_purpose::STANDARD
        .decode(&public)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&public))
        .map_err(|_| Error::http(502, "Loomy 登录公钥编码无效"))?;
    let key = RsaPublicKey::from_public_key_der(&der)
        .map_err(|_| Error::http(502, "Loomy 登录公钥格式无效"))?;
    if !(1024..=4096).contains(&key.n().bits()) || password.len() > key.size().saturating_sub(11) {
        return Err(Error::http(400, "Loomy 登录公钥不支持此密码长度"));
    }
    let encrypted = key
        .encrypt(&mut HostRandom, Pkcs1v15Encrypt, password.as_bytes())
        .map_err(|_| Error::http(502, "Loomy 密码加密失败，未提交登录"))?;
    let encrypted = base64::engine::general_purpose::STANDARD.encode(encrypted);
    let body = json!({ "base": base()?, "param": { "loginid": login,
        "password": encrypted, "rcode": rcode, "type": 1, "expire": 14 * 24 * 3600 } });
    let payload = client::signed_post(
        "/login/account/byPwd",
        &body,
        "账号密码登录",
        &[login, password, &rcode, &encrypted],
    )?;
    errors::ensure_success(&payload, "账号密码登录", password)?;
    let data = self::data(&payload)?;
    let session = account::token(data)?;
    let uid = account::identifier(data, &["userId", "userid", "user_id"]);
    let phone = account::identifier(data, &["phone", "loginid"]);
    account::normalized(&session, &uid, &phone, name)
}
