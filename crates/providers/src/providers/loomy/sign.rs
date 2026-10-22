//! Loomy 账号（CAccount）请求签名：HMAC-SHA1（复刻客户端 `electron/xfyun/sign.js`）。
//!
//! ── 待签字符串（9 段，`\n` 连接，与客户端 `generateSignature` 逐字同构）──
//! ```text
//!   METHOD
//!   ESCAPED_PATH          RFC3986 §3.3（逐段编码后再替换 !'()* ）
//!   ESCAPED_QUERY         本项目所有账号接口都无 query，恒为空串
//!   CONTENT_MD5           base64(md5(body 字节串))；无 body 时为空串
//!   CONTENT_TYPE          "application/json"
//!   DATE                  UTC 的 `Tue, 07 Oct 2026 08:00:00 GMT` 形态
//!   NONCE                 UUID v4（带连字符）
//!   SIGNED_HEADERS        只列 `x-` 前缀头；本项目不带，恒为空串
//!   CANONICALIZED_HEADERS 同上，恒为空串
//! ```
//! 第二、九段的两处易错点：路径要有前导 `/` 且**不以 `/` 结尾**；末两段是
//! 空串，因此拼接结果在 nonce 之后以两个 `\n` 结尾 —— 这里用数组 join 而不是
//! 手写 format，正是为了让段数与分隔符数量不可能被写错。
//!
//! ── Content-MD5 必须覆盖**实际发出去的字节**──────────────────
//! `signed_headers` 与出网层（`crate::http::post_json`）都走
//! `Value::to_string()`（紧凑序列化），两处必须保持同一序列化口径 —— 换任何
//! 一方都会让 MD5 对不上，上游报签名错误，而错误信息不会指出是哪个环节。
//!
//! ── HMAC-SHA1 不是安全边界 ──────────────────────────────────
//! 这是客户端指纹复刻（密钥随安装包分发），目标是与官方客户端行为一致，
//! 不是密码学意义上的完整性保证。

use base64::Engine;
use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use serde_json::Value;
use sha1::Sha1;

use super::endpoints;

/// 业务接口的 Content-Type（也是待签串第 5 段的取值）
const CONTENT_TYPE: &str = "application/json";

/// RFC3986 转义一个路径段。
///
/// 保留字符集：`A-Z a-z 0-9 - _ . ~`；其余一律 `%XX`（大写十六进制）。
/// 当前所有账号接口路径都只含字母与 `/`，转义结果与原串相同。
fn escape_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~') {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 构建 ESCAPED_PATH：逐段转义、以 `/` 连接；去掉尾部斜杠（客户端同款）。
fn build_escaped_path(path: &str) -> String {
    let mut clean = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    if clean.len() > 1 && clean.ends_with('/') {
        clean.pop();
    }
    clean
        .split('/')
        .map(escape_path_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// UTC 时间字符串（对应 JS `new Date().toUTCString()`）：
/// `Sun, 06 Oct 2026 12:34:56 GMT`
fn utc_date_string() -> String {
    chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// UUID v4 形态的 Nonce（客户端签名用的是**带连字符**的 randomUUID 原样值）。
///
/// 随机源不可用时退化成「纳秒时钟 + 进程内计数」—— 它只用于让每次请求的
/// 一次性串不同（防重放口径由服务端与时间窗共同保证），不是安全凭证。
fn new_nonce() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static FALLBACK_COUNTER: AtomicU64 = AtomicU64::new(1);
        let counter = FALLBACK_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0);
        let mixed = nanos
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(counter);
        for (index, slot) in bytes.iter_mut().enumerate() {
            *slot = (mixed >> ((index % 8) * 8)) as u8;
        }
    }
    // RFC4122 v4：版本位与变体位
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

/// base64(md5(字节串))；空串输入返回空串（对应客户端 `contentMD5('')`）
fn content_md5(body_string: &str) -> String {
    if body_string.is_empty() {
        return String::new();
    }
    let digest = Md5::digest(body_string.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(digest)
}

/// 生成一次请求的签名头集合：`Authorization` / `Date` / `Nonce` / `Content-MD5`。
///
/// `body` 为 `None` 时按空 body 处理（MD5 段为空串）。序列化口径必须与出网层
/// 完全一致（`Value::to_string()`，见模块头）。
pub fn signed_headers(
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Vec<(String, String)>, String> {
    let body_string = match body {
        Some(value) => value.to_string(),
        None => String::new(),
    };
    let md5 = content_md5(&body_string);
    let date = utc_date_string();
    let nonce = new_nonce();
    // 9 段，`\n` 连接（末两段恒为空串，见模块头）
    let string_to_sign = [
        method.to_uppercase(),
        build_escaped_path(path),
        String::new(),
        md5.clone(),
        CONTENT_TYPE.to_string(),
        date.clone(),
        nonce.clone(),
        String::new(),
        String::new(),
    ]
    .join("\n");

    let secret = endpoints::access_key_secret();
    let mut mac = Hmac::<Sha1>::new_from_slice(secret.as_bytes()).map_err(|_| {
        "Loomy 签名初始化失败（密钥长度非法，请检查 LOOMY_XFYUN_ACCESS_KEY_SECRET）".to_string()
    })?;
    mac.update(string_to_sign.as_bytes());
    let signature = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

    let mut headers = vec![
        (
            "Authorization".to_string(),
            format!("account {}:{}", endpoints::access_key_id(), signature),
        ),
        ("Date".to_string(), date),
        ("Nonce".to_string(), nonce),
    ];
    if !md5.is_empty() {
        headers.push(("Content-MD5".to_string(), md5));
    }
    Ok(headers)
}
