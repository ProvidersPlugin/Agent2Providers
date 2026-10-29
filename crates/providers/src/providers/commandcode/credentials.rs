//! Command Code 凭证：**单个粘贴的 API Key**（无 OAuth、无设备码、无续期）。
//!
//! ── 上游的凭证形态 ──────────────────────────────────────────
//! 一个以 `user_` 开头的字符串，从 <https://commandcode.ai/studio> 或本机
//! `~/.commandcode/auth.json` 自取。插件侧只需要把它透传成
//! `Authorization: Bearer <key>`。
//!
//! ── 在插件架构里的落点（与网关版的一处实质差别）─────────────────
//! 网关版从 `store.commandcode_account_record()` 读记录；这里从
//! `ctx.store_get()` 读 —— 记录的 `secret` 是插件自己写进去的那份 JSON
//! （见 `normalize_account`），所以取值链只在 `secret` 里找。
//!
//! ── 校验：本地形态检查为主，线上探活是 best-effort ───────────────
//! 形态检查**必定执行**（本地、零网络），线上探活**尽力而为** —— 只有拿到
//! 401/403 才判无效，网络失败 / 5xx / 未知一律按「暂时判不出来」放行
//! （否则一次上游抖动就会让用户加不进账号）。

use serde_json::Value;

use super::endpoints;
use crate::http;

/// Key 前缀（上游要求的唯一硬性形态）
pub const KEY_PREFIX: &str = "user_";

/// Key 字符集（`user_` 之后是 `[a-zA-Z0-9_-]+`）
fn is_key_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'
}

/// 形态检查：以 `user_` 开头、其后至少一个合法字符、且整串都在字符集内。
pub fn shape_ok(key: &str) -> bool {
    let Some(rest) = key.trim().strip_prefix(KEY_PREFIX) else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(is_key_char)
}

/// 展示用的脱敏形态（只露前缀与尾 4 位）。
pub fn masked(key: &str) -> String {
    let chars: Vec<char> = key.trim().chars().collect();
    if chars.len() <= 12 {
        return format!("{KEY_PREFIX}…");
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{KEY_PREFIX}…{tail}")
}

/// 从一条取值链里挑第一个非空字符串（记录可能被手工编辑过，宽容读取）
fn pick<'a>(record: &'a Value, keys: &[&str]) -> String {
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

/// 从账号的 `secret` 里取 API Key（**唯一入口**）。
///
/// 字段兼容：`apiKey`（本插件落盘名）/ `accessToken` / `token` / `key` ——
/// 用户从别处粘过来的 JSON 里字段名并不统一，宽容读取比报错更实用。
pub fn api_key_of(secret: &Value) -> Result<String, String> {
    let api_key = pick(secret, &["apiKey", "accessToken", "token", "key"]);
    if api_key.is_empty() {
        return Err("Command Code 账号缺少 API Key，请重新粘贴（user_ 开头）".to_string());
    }
    if !shape_ok(&api_key) {
        return Err(format!(
            "Command Code API Key 形态不对（应为 user_ 开头、只含字母数字与 _ -，当前 {}）：\
             请在 commandcode.ai/studio 重新获取",
            masked(&api_key)
        ));
    }
    Ok(api_key)
}

/// 线上探活的结果
pub enum KeyCheck {
    /// 上游认这个 key（2xx）
    Valid,
    /// 上游明确拒绝（401/403）—— key 无效 / 已失效
    Invalid(String),
    /// 这次判不出来（网络失败 / 其他状态码）—— **按可用放行**
    Unknown(String),
}

/// `GET /alpha/billing/credits` 探活（401/403 即无效）。
///
/// 为什么用 billing 而不是随便一个需要鉴权的端点：`/alpha/generate` 会把
/// 一次**真实生成**的额度花掉；billing 是只读的额度查询，对账号零副作用。
pub async fn probe_key(api_key: &str, gateway_proxy: &str) -> KeyCheck {
    let mut headers = endpoints::cli_headers();
    headers.extend(endpoints::auth_headers(api_key));
    let client = http::control_client(None, gateway_proxy);
    match http::get_json(
        &client,
        &endpoints::url(endpoints::BILLING_CREDITS_PATH),
        &headers,
    )
    .await
    {
        Ok((status, _)) => {
            if status == 401 || status == 403 {
                KeyCheck::Invalid(format!("上游返回 HTTP {status}"))
            } else if (200..300).contains(&status) {
                KeyCheck::Valid
            } else {
                KeyCheck::Unknown(format!("上游返回 HTTP {status}"))
            }
        }
        Err(error) => KeyCheck::Unknown(format!("探活请求失败：{error}")),
    }
}

/// 令牌尾 4 字符（公开形态用；按 `char` 取，多字节输入不得 panic）
pub fn tail_of(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let start = chars.len().saturating_sub(4);
    chars[start..].iter().collect()
}
