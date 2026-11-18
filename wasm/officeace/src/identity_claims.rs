//! JWT 仅用于显示名和 principal 提取，绝不替代上游权限校验；沿用内层 user_profile 兜底。

use super::util::text;
use agent2api_wasm_sdk::Value;
use base64::{
    engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD},
    Engine,
};

#[derive(Default)]
pub struct Identity {
    pub name: String,
    pub principal: String,
}

fn decode(encoded: &str) -> Option<Value> {
    let encoded = encoded.trim().trim_end_matches('=');
    for bytes in [
        URL_SAFE_NO_PAD.decode(encoded),
        STANDARD_NO_PAD.decode(encoded),
    ]
    .into_iter()
    .flatten()
    {
        if let Ok(value) = serde_json::from_slice(&bytes) {
            return Some(value);
        }
    }
    None
}

fn identity(token: &str) -> Identity {
    let claims = token
        .split('.')
        .nth(1)
        .and_then(decode)
        .unwrap_or(Value::Null);
    let profile = claims["user_profile"]
        .as_str()
        .and_then(decode)
        .unwrap_or(Value::Null);
    let mut name = text(&claims, &["preferred_username", "name"]);
    if name.is_empty() {
        name = text(&profile, &["account_name"]);
    }
    let mut principal = text(&profile, &["principal_id"]);
    if principal.is_empty() {
        principal = text(&claims, &["principal_id"]);
    }
    Identity { name, principal }
}

pub fn response(payload: &Value) -> Identity {
    let mut fallback = Identity::default();
    for key in ["id_token", "refresh_token"] {
        let identity = identity(payload[key].as_str().unwrap_or(""));
        if !identity.name.is_empty() {
            return identity;
        }
        if fallback.principal.is_empty() {
            fallback = identity;
        }
    }
    fallback
}
