//! 登录用的设备密钥对（EC P-256）。
//!
//! ── 为什么要专门生成一对 ──────────────────────────────────
//! 官方客户端每次登录都现生成一套 P-256，把 **SPKI 公钥 PEM** 塞进
//! `ExchangeToken` 的 `DeviceInfo.DevicePublicKey`，服务端把它与该次登录的
//! `DeviceID` / `MachineID` 绑在一起。参考实现注明：**空值会被判成
//! HTTP 401 / 业务码 20405**（`2xxxx` 那族是凭据/设备绑定拒绝，
//! 2026-09-04 实测），所以这不是"可选的元数据"，少了就登不进来。
//!
//! 私钥同样要落盘（`auth.devicePrivateKey`）：参考实现里它**只写不读**，
//! 但重建凭据时必须保住 —— 把它抹掉等于把这台设备的绑定关系弄断，
//! 而那条链上没有任何一处会告诉你为什么突然 401 了。
//!
//! PEM 的行宽按 Go `encoding/pem` 的 64 字符折行，两边产物可以直接对读。

use base64::{engine::general_purpose::STANDARD, Engine as _};
use p256::pkcs8::{EncodePrivateKey, EncodePublicKey};

/// 一对设备密钥（公钥进请求，私钥进凭据）。
#[derive(Clone, Debug)]
pub struct DeviceKeyPair {
    /// `-----BEGIN PUBLIC KEY-----`（SPKI / DER）
    pub public_pem: String,
    /// `-----BEGIN PRIVATE KEY-----`（PKCS#8 / DER）
    pub private_pem: String,
}

/// 生成一套新的设备密钥。
///
/// 标量取自系统随机源；落在 `[1, n-1]` 之外时**重取**（概率约 2^-128，
/// 但"取一次就假定成功"在密码学路径上是不允许的写法）。
pub fn generate_device_key_pair() -> Option<DeviceKeyPair> {
    let mut attempts = 0;
    while attempts < 16 {
        attempts += 1;
        let mut bytes = [0u8; 32];
        if getrandom::getrandom(&mut bytes).is_err() {
            return None;
        }
        let scalar = p256::FieldBytes::from_slice(&bytes);
        let Ok(key) = p256::ecdsa::SigningKey::from_bytes(scalar) else {
            continue; // 标量落在 [1, n-1] 之外，重取
        };
        // 自己拼 PEM：pkcs8 0.10 的 `to_*_pem` 要 `pkcs8/pem` 特性（p256 的 `pem`
        // 只转到 `elliptic-curve/pem`，没打开它），而 Go 侧 `encoding/pem` 的
        // 行宽是写死的 64 字符 + 结尾换行 —— 手拼反而把这份格式钉在代码里，
        // 两边产物字节级可互读。
        let public = pem("PUBLIC KEY", key.verifying_key().to_public_key_der().ok()?.as_bytes());
        let private = pem("PRIVATE KEY", key.to_pkcs8_der().ok()?.as_bytes());
        return Some(DeviceKeyPair { public_pem: public, private_pem: private });
    }
    None
}

/// DER → Go `encoding/pem` 同格式的 PEM（64 字符折行，首尾各带换行）。
fn pem(label: &str, der: &[u8]) -> String {
    let encoded = STANDARD.encode(der);
    let mut out = String::with_capacity(encoded.len() + 64);
    out.push_str("-----BEGIN ");
    out.push_str(label);
    out.push_str("-----\n");
    for chunk in encoded.as_bytes().chunks(64) {
        // base64 表全是 ASCII，分片不会切出非法 UTF-8 序列。
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push('\n');
    }
    out.push_str("-----END ");
    out.push_str(label);
    out.push_str("-----\n");
    out
}

