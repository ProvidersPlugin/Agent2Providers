//! 账号公开视图只含令牌尾部；本机账号读取当前文件，不缓存凭证到账号库。

use serde_json::{json, Value};

use super::{credentials, jwt};

pub(super) fn account_view(secret: &Value, fallback_name: &str) -> Value {
    let desktop = secret.get("desktop").and_then(Value::as_bool) == Some(true);
    let live = if desktop {
        credentials::desktop_credentials().ok()
    } else {
        None
    };
    let token = live
        .as_ref()
        .map(|credentials| credentials.token.as_str())
        .unwrap_or_else(|| {
            secret
                .get("accessToken")
                .and_then(Value::as_str)
                .unwrap_or("")
        });
    let user_id = secret.get("userId").and_then(Value::as_str).unwrap_or("");
    let explicit = secret
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let name = if !explicit.is_empty() {
        explicit.to_string()
    } else if !fallback_name.trim().is_empty() {
        fallback_name.trim().to_string()
    } else if !user_id.is_empty() {
        format!("账号 {user_id}")
    } else {
        "小浣熊账号".to_string()
    };
    let can_refresh = live
        .as_ref()
        .map(credentials::RaccoonCredentials::can_refresh)
        .unwrap_or_else(|| {
            !desktop
                && !jwt::is_iam_token(token)
                && !crate::store::pick(secret, &["refreshToken", "refresh_token"]).is_empty()
        });
    let mut view = json!({
        "name": name,
        "uid": user_id,
        "userId": user_id,
        "tokenTail": crate::store::token_tail(token),
        "hasSession": !token.is_empty(),
        "canRefresh": can_refresh,
    });
    let expiry = live
        .as_ref()
        .and_then(|credentials| credentials.expires_at)
        .or_else(|| secret.get("expiresAt").and_then(Value::as_f64));
    if let Some(expiry) = expiry {
        view["expiresAt"] = json!(expiry);
    }
    for key in ["officeIdentity", "officeOrgName", "officeOrgRole"] {
        let value = secret
            .get(key)
            .or_else(|| secret.get(&snake_case(key)))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !value.is_empty() {
            view[key] = json!(value);
        }
    }
    view
}

pub(super) fn identity_of(secret: &Value) -> String {
    let user_id = crate::store::pick(secret, &["userId"]);
    if user_id.is_empty() {
        String::new()
    } else {
        format!("user-{user_id}")
    }
}

fn snake_case(key: &str) -> String {
    let mut out = String::with_capacity(key.len() + 4);
    for ch in key.chars() {
        if ch.is_ascii_uppercase() {
            out.push('_');
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}
