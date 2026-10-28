//! 账号：`wps_sid` Cookie 的解析、落盘形态与公开视图。
//!
//! ── 为什么只存 `wps_sid` 一个值 ─────────────────────────────
//! 云端的两个平面（agent-runtime 与积分/任务）都只认 `Cookie: wps_sid=…`；
//! 桌面登录态里另外那些键（`kso_sid` / `uid` / `rtk`…）对反代没有用处。
//! 用户通常会把整段 Cookie 粘进来，这里负责从中取出 `wps_sid`，其余丢弃。

use agent2api_provider_protocol as proto;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use agent2api_provider_sdk::PluginCtx;

use super::constants;

/// 从粘贴内容里抽 `wps_sid`：支持裸值 / `wps_sid=…` / 整段 Cookie（`a=b; c=d`）。
pub fn extract_sid(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains('=') || trimmed.contains(';') {
        for part in trimmed.split(';') {
            let part = part.trim();
            if let Some(value) = part.strip_prefix("wps_sid=") {
                let value = value.trim().trim_matches('"');
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
        // 整段 Cookie 里没有 wps_sid：不猜
        return None;
    }
    // 裸值：长度像会话 Cookie 才收（太短的可能是误粘的别的令牌）
    (trimmed.len() >= 16).then(|| trimmed.to_string())
}

/// 从整段 Cookie 里顺带取 `uid`（只用于展示；裸 sid 时没有）。
pub fn extract_uid(text: &str) -> String {
    for part in text.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix("uid=") {
            return value.trim().trim_matches('"').to_string();
        }
    }
    String::new()
}

/// 稳定身份：sid 的 sha256 前 16 位十六进制（同一 Cookie 恒定、不同账号不同）。
pub fn identity_of(sid: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(sid.as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

fn tail(sid: &str) -> String {
    sid.chars().rev().take(4).collect::<Vec<char>>().into_iter().rev().collect()
}

/// 把一次「添加账号」的输入归一成（secret, public, identity）。
pub fn normalize(input: &proto::NormalizeInput) -> Result<proto::NormalizedAccount, String> {
    let payload = match &input.source {
        proto::NormalizeSource::Manual { payload } => payload.clone(),
        proto::NormalizeSource::FileText { content, .. } => {
            // 允许把整段 Cookie 存成 txt 再导入
            json!({ "cookie": content })
        }
        proto::NormalizeSource::FileBase64 { .. } => {
            return Err("灵犀不支持二进制凭证文件（请粘贴 Cookie 文本）".to_string())
        }
        proto::NormalizeSource::Desktop => {
            // 本机桌面登录态在 Windows 的 DPAPI 密文里，插件进程读不到；
            // 这条来源不提供（由用户粘贴 Cookie）。
            return Err("灵犀暂不支持「导入本机客户端登录态」，请粘贴 wps_sid Cookie".to_string());
        }
    };

    let raw = ["cookie", "wps_sid", "sid", "token", "accessToken"]
        .iter()
        .find_map(|key| payload.get(*key).and_then(Value::as_str))
        .unwrap_or("");
    let sid = extract_sid(raw)
        .ok_or_else(|| "没有在输入里找到 wps_sid（可粘贴整段 Cookie 或 wps_sid 的值）".to_string())?;
    let uid = extract_uid(raw);
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("WPS 灵犀")
        .to_string();

    Ok(proto::NormalizedAccount {
        secret: json!({ "sid": sid, "uid": uid, "name": name }),
        public: json!({
            "name": name,
            "uid": uid,
            "hasCookie": true,
            "cookieTail": tail(&sid),
        }),
        identity: identity_of(&sid),
        desktop: false,
    })
}

/// 从凭证里重新生成公开视图（`account_view`）。
pub fn view(secret: &Value) -> Result<proto::AccountView, String> {
    let sid = secret.get("sid").and_then(Value::as_str).unwrap_or("");
    if sid.is_empty() {
        return Err("账号缺少 wps_sid".to_string());
    }
    Ok(json!({
        "name": secret.get("name").and_then(Value::as_str).unwrap_or("WPS 灵犀"),
        "uid": secret.get("uid").and_then(Value::as_str).unwrap_or(""),
        "hasCookie": true,
        "cookieTail": tail(sid),
    }))
}

/// 取一个账号的 sid：`account` 为空串时用本家第一个账号。
pub async fn sid_for(ctx: &PluginCtx, account: &str) -> Result<String, String> {
    let record = if account.trim().is_empty() {
        ctx.store_list(constants::ID)
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .next()
            .ok_or_else(|| "还没有可用的 WPS 灵犀账号（请先粘贴 Cookie 添加）".to_string())?
    } else {
        ctx.store_get(constants::ID, account)
            .await
            .map_err(|error| error.to_string())?
    };
    let sid = record
        .secret
        .get("sid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if sid.is_empty() {
        return Err("账号缺少 wps_sid，请重新添加".to_string());
    }
    Ok(sid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_sid_from_raw_and_full_cookie() {
        assert_eq!(extract_sid("abc1234567890abcdef").as_deref(), Some("abc1234567890abcdef"));
        assert_eq!(
            extract_sid("kso_sid=x; wps_sid=SID1234567890abcdef; uid=42").as_deref(),
            Some("SID1234567890abcdef")
        );
        assert_eq!(extract_sid("short"), None);
        assert_eq!(extract_sid("a=b; c=d"), None);
    }

    #[test]
    fn identity_is_stable_and_distinct() {
        assert_eq!(identity_of("sid-1"), identity_of("sid-1"));
        assert_ne!(identity_of("sid-1"), identity_of("sid-2"));
        assert_eq!(identity_of("sid-1").len(), 16);
    }

    #[test]
    fn view_hides_the_cookie_body() {
        let view = view(&json!({ "sid": "SID1234567890", "name": "灵犀" })).expect("view");
        assert_eq!(view.get("cookieTail").and_then(Value::as_str), Some("7890"));
        assert!(view.get("sid").is_none());
    }
}
