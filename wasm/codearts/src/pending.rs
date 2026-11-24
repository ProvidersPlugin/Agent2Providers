//! 续期结果的耐久暂存：两次 `account.update` 模拟「先落盘、再激活」。
//!
//! `refresh_token` 是一次性的：上游换证成功那一刻旧串就作废了。宿主只有
//! `account.update` 这一条比较再写，没有独立的 journal 表；如果只在换证后
//! 写一次，写回返回 false（记录被并发改动）、写库失败或进程退出都会让新令牌
//! 只留在内存里，下一次刷新只能拿已经作废的旧串去碰运气。
//!
//! 这里先用一次 CAS 把新凭据放进账号 secret 的私有暂存块（**活动字段一个都
//! 不动**），再用第二次 CAS 把块里的补丁并回活动字段并删掉块。第二次没落地
//! 时块还在盘上，下一次读账号先补激活；块用旧 refresh token 的截断指纹绑定，
//! 账号被重新导入或另一轮刷新已落盘时指纹对不上，直接判陈旧丢弃。
//!
//! 与原生插件 `crates/providers/src/providers/codearts/refresh.rs` 的
//! `_agent2apiPendingRefresh` 同一套语义；那个仓库没有 WASM 侧的两阶段写回，
//! 两边各自实现是因为写回 API 不同（`store.update` vs `account.update`）。

use agent2api_wasm_sdk::{host, json, Value};
use sha2::{Digest, Sha256};

use crate::{account, credentials::Credential, util};

const PENDING_KEY: &str = "_agent2apiPendingRefresh";
const PENDING_VERSION: u64 = 1;

pub enum Recovery {
    None,
    Promoted,
    Waiting,
}

struct Pending {
    base_refresh: String,
    patch: Value,
}

/// 账号 secret 里是否还留着上一轮没激活的续期结果。
pub fn has_pending(secret: &Value) -> bool {
    secret.get(PENDING_KEY).is_some()
}

/// 比较「活动凭据」是否仍是同一份：忽略暂存块本身。
///
/// 续期写回会先后落两次 CAS，中间的短暂状态只有暂存块不同；并发的只读请求
/// 不该因为这一瞬间被判成「账号已被外部修改」。
pub fn same_active_secret(left: &Value, right: &Value) -> bool {
    fn strip(value: &Value) -> Value {
        let mut cleaned = value.clone();
        if let Some(fields) = cleaned.as_object_mut() {
            fields.remove(PENDING_KEY);
        }
        cleaned
    }
    strip(left) == strip(right)
}

fn fingerprint(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut out = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn pending_of(secret: &Value) -> Option<std::result::Result<Pending, &'static str>> {
    let marker = secret.get(PENDING_KEY)?;
    let Some(fields) = marker.as_object() else {
        return Some(Err("暂存块不是对象"));
    };
    if fields.get("version").and_then(Value::as_u64) != Some(PENDING_VERSION) {
        return Some(Err("暂存块版本不支持"));
    }
    let Some(base_refresh) = fields
        .get("baseRefresh")
        .and_then(Value::as_str)
        .filter(|text| text.len() == 32 && text.chars().all(|value| value.is_ascii_hexdigit()))
    else {
        return Some(Err("暂存块缺少旧 refresh token 指纹"));
    };
    let Some(patch) = fields.get("patch").filter(|value| value.is_object()) else {
        return Some(Err("暂存块缺少凭据补丁"));
    };
    Some(Ok(Pending {
        base_refresh: base_refresh.to_string(),
        patch: patch.clone(),
    }))
}

/// 第一次 CAS：把补丁放进暂存块，活动凭据保持旧值。
///
/// 返回 `Some(journal_secret)` = 已耐久落盘；`None` = 没写成（调用方退回
/// 原来的单次写回）。
pub fn journal(secret: &Value, base_refresh: &str, patch: &Value) -> Option<Value> {
    if base_refresh.trim().is_empty() || !patch.is_object() {
        return None;
    }
    let marker = json!({
        "version": PENDING_VERSION,
        "baseRefresh": fingerprint(base_refresh),
        "patch": patch,
        "at": util::now().ok()?,
    });
    let mut next = secret.clone();
    next.as_object_mut()?.insert(PENDING_KEY.to_string(), marker);
    let public = account::view(&next).ok()?;
    let updated = host(
        "account.update",
        json!({ "expect": secret, "secret": next.clone(), "public": public }),
    )
    .ok()?;
    (updated.get("updated").and_then(Value::as_bool) == Some(true)).then_some(next)
}

/// 第二次 CAS：把暂存块换成正凭据。`false` = 记录已变化，块仍在盘上。
pub fn activate(journal: &Value, secret: &Value, public: &Value) -> bool {
    host(
        "account.update",
        json!({ "expect": journal, "secret": secret, "public": public }),
    )
    .ok()
    .is_some_and(|value| value.get("updated").and_then(Value::as_bool) == Some(true))
}

/// 读账号前尝试补激活；任何一步不确定都保留暂存块，等下一轮再试。
pub fn recover(record: &Value) -> Recovery {
    let secret = &record["secret"];
    let Some(found) = pending_of(secret) else {
        return Recovery::None;
    };
    let pending = match found {
        Ok(pending) => pending,
        Err(_) => {
            clear(secret);
            return Recovery::None;
        }
    };
    let current = match Credential::parse(secret) {
        Ok(credential) => credential,
        Err(_) => return Recovery::Waiting,
    };
    if fingerprint(&current.refresh_token) != pending.base_refresh {
        // 账号已经换过授权（重新登录/重新导入）或另一轮刷新已落盘：这份结果
        // 不再属于当前凭据，删掉，绝不覆盖新值。
        clear(secret);
        return Recovery::None;
    }
    let mut next = secret.clone();
    if let Some(fields) = next.as_object_mut() {
        fields.remove(PENDING_KEY);
    }
    apply_patch(&mut next, &pending.patch);
    let public = match account::view(&next) {
        Ok(public) => public,
        Err(_) => {
            // 补丁本身不合法（理论上来自已验证的换证响应）：丢掉，别让坏块
            // 永久挡住后续续期。
            clear(secret);
            return Recovery::None;
        }
    };
    match host(
        "account.update",
        json!({ "expect": secret, "secret": next, "public": public }),
    ) {
        Ok(value) if value.get("updated").and_then(Value::as_bool) == Some(true) => {
            Recovery::Promoted
        }
        _ => Recovery::Waiting,
    }
}

fn clear(secret: &Value) {
    let mut cleaned = secret.clone();
    let Some(fields) = cleaned.as_object_mut() else {
        return;
    };
    fields.remove(PENDING_KEY);
    let Ok(public) = account::view(&cleaned) else {
        return;
    };
    let _ = host(
        "account.update",
        json!({ "expect": secret, "secret": cleaned, "public": public }),
    );
}

/// 换证响应要写回的字段补丁（与原生 `apply_refreshed_tokens` 同一组键）。
pub fn token_patch(
    access_key_id: &str,
    secret_access_key: &str,
    security_token: &str,
    expiration: &str,
    expires_at_ms: i64,
    refresh_token: &str,
) -> Value {
    json!({
        "set": {
            "access_key_id": access_key_id,
            "accessToken": access_key_id,
            "secret_access_key": secret_access_key,
            "security_token": security_token,
            "refresh_token": refresh_token,
            "refreshToken": refresh_token,
            "expires_at": expiration,
            "expiration": expiration,
            "expiresAt": expires_at_ms,
        },
        "remove": ["needsRelogin", "needsReloginReason"],
    })
}

/// 把补丁应用到根对象与两个固定嵌套入口；未知包装不动，避免洗掉账号侧字段。
pub fn apply_patch(secret: &mut Value, patch: &Value) {
    let set = patch.get("set").and_then(Value::as_object);
    let remove = patch.get("remove").and_then(Value::as_array);
    let apply = |fields: &mut serde_json::Map<String, Value>| {
        if let Some(set) = set {
            for (key, value) in set {
                fields.insert(key.clone(), value.clone());
            }
        }
        if let Some(remove) = remove {
            for key in remove.iter().filter_map(Value::as_str) {
                fields.remove(key);
            }
        }
    };
    let Some(root) = secret.as_object_mut() else {
        return;
    };
    apply(root);
    for key in ["codearts_provider_credential", "credential", "auth"] {
        if let Some(Value::Object(nested)) = root.get_mut(key) {
            apply(nested);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch() -> Value {
        token_patch(
            "AK",
            "SK",
            "STS",
            "2030-01-01T00:00:00Z",
            1_900_000_000_000,
            "new-refresh",
        )
    }

    #[test]
    fn fingerprint_is_stable_and_separates_tokens() {
        let first = fingerprint("old-refresh");
        assert_eq!(first.len(), 32);
        assert!(first.chars().all(|value| value.is_ascii_hexdigit()));
        assert_eq!(first, fingerprint("old-refresh"));
        assert_ne!(first, fingerprint("new-refresh"));
    }

    #[test]
    fn pending_marker_round_trips_with_the_old_refresh_binding() {
        let marker = json!({
            "version": PENDING_VERSION,
            "baseRefresh": fingerprint("old-refresh"),
            "patch": patch(),
            "at": 1,
        });
        let secret = json!({
            "_agent2apiPendingRefresh": marker,
            "refresh_token": "old-refresh",
        });
        let parsed = pending_of(&secret);
        assert!(matches!(parsed, Some(Ok(_))));
        if let Some(Ok(pending)) = parsed {
            assert_eq!(pending.base_refresh, fingerprint("old-refresh"));
            assert!(pending.patch.is_object());
        }
    }

    #[test]
    fn patch_updates_nested_credentials_and_keeps_account_fields() {
        let mut secret = json!({
            "refresh_token": "old",
            "extra": "keep",
            "needsRelogin": true,
            "credential": {
                "refresh_token": "old",
                "needsRelogin": true,
                "other": 1,
            }
        });
        apply_patch(&mut secret, &patch());
        assert_eq!(secret["refresh_token"], json!("new-refresh"));
        assert_eq!(secret["extra"], json!("keep"));
        assert!(secret.get("needsRelogin").is_none());
        assert_eq!(secret["credential"]["refresh_token"], json!("new-refresh"));
        assert_eq!(secret["credential"]["other"], json!(1));
        assert!(secret["credential"].get("needsRelogin").is_none());
    }

    #[test]
    fn active_secret_comparison_ignores_the_pending_block() {
        let active = json!({ "refresh_token": "old", "extra": "keep" });
        let mut journal = active.clone();
        if let Some(fields) = journal.as_object_mut() {
            fields.insert("_agent2apiPendingRefresh".to_string(), json!({}));
        }
        assert!(same_active_secret(&active, &journal));
        assert!(!same_active_secret(&active, &json!({ "refresh_token": "new" })));
    }
}
