use agent2api_provider_protocol as proto;
use serde_json::Value;

use super::{credentials, login};

pub(super) fn from_file(
    content: &str,
    name: Option<&str>,
) -> Result<proto::NormalizedAccount, String> {
    let payload: Value = serde_json::from_str(content).map_err(|_| {
        "Cline 凭证文件不是合法 JSON，请选择 providers.json 或单账号 JSON 文件".to_string()
    })?;
    let credentials = if payload.get("providers").is_some() {
        credentials::credentials_from_desktop_settings(&payload)
    } else {
        credentials::credentials_from_record(&payload)
    }
    .map_err(|error| error.message)?;
    let mut account = login::account_of(&credentials.access_token, &credentials.refresh_token);
    if !credentials.account.trim().is_empty() {
        account.identity = credentials.account.clone();
        account.secret["account"] = Value::String(credentials.account.clone());
        account.public["uid"] = Value::String(credentials.account);
    }
    if let Some(expires_at) = credentials.expires_at {
        let value = serde_json::json!(expires_at);
        account.secret["expiresAt"] = value.clone();
        account.public["expiresAt"] = value;
    }
    let name = name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(credentials.name.trim());
    if !name.is_empty() {
        let name = crate::json::truncate_text(name, 100);
        account.secret["displayName"] = Value::String(name.clone());
        account.public["name"] = Value::String(name);
    }
    Ok(account)
}
