//! ModelScope 凭证：**只有一条来源** —— 账号记录里的 `ms-` 令牌。
//!
//! ── 为什么只要一个字段（实测依据，见 `region.rs` 的模块头）────────
//! 同一个 `ms-...` 令牌既作 `Authorization: Bearer` 调推理，又作
//! `Cookie: m_session_id=<key>` 调网页站接口 —— 两条链路实测都是 200。
//!
//! ── 没有续期 ────────────────────────────────────────────────
//! 魔搭的 `ms-` 令牌是**长期有效**的（上游没有任何 refresh / renew 接口）。
//! 因此 `refresh = false`，令牌失效只能重新去魔搭控制台复制一个。
//!
//! ── 备份字段 ────────────────────────────────────────────────
//! `accessToken` 是网关的落盘名；粘贴形态另外兼容 `apiKey` / `token` /
//! `session` —— 用户从别处复制过来的 JSON 里字段名并不统一。

use serde_json::Value;

/// 从一条取值链里挑第一个非空字符串（记录可能被手工编辑过，宽容读取）
fn pick(record: &Value, keys: &[&str]) -> String {
    for key in keys {
        let value = record
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !value.is_empty() {
            return value.to_string();
        }
    }
    String::new()
}

/// 从账号的 `secret` 里取令牌（**唯一入口**）。
pub fn token_of(secret: &Value) -> Result<String, String> {
    let token = pick(secret, &["accessToken", "apiKey", "token", "session"]);
    if token.is_empty() {
        return Err("ModelScope 账号缺少令牌，请重新粘贴".to_string());
    }
    Ok(token)
}

/// 从**上传的 payload** 里取令牌（添加路径专用：先取出来去探活，再落账号）。
///
/// 与 [`token_of`] 的取值链**逐字相同** —— 两处口径分叉的话，「能探活成功
/// 却落不进账号」或反之都会变成难查的静默问题。
pub fn token_from_payload(payload: &Value) -> String {
    pick(payload, &["accessToken", "apiKey", "token", "session"])
}

/// 令牌尾 4 字符（公开形态与默认备注名用）。
///
/// 按 `char` 取而不按字节切：粘贴形态的输入框不做格式校验，一个带汉字的
/// 误粘贴就能让按字节回切落在多字节字符中间。
pub fn token_tail(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let start = chars.len().saturating_sub(4);
    chars[start..].iter().collect()
}
