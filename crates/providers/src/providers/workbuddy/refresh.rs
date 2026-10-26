//! WorkBuddy 续期：单飞覆盖凭证复查、上游换发与比较再写。
//!
//! refresh token 换发后旧值失效，只有新凭证已经落盘才能向等待者报告成功。
//! 写回冲突时保留最新的非鉴权字段；凭证已被替换则不覆盖新登录态。

use std::sync::OnceLock;

use agent2api_provider_protocol::WireProxy;
use agent2api_provider_sdk::PluginCtx;
use serde_json::Value;

use crate::errors::GatewayError;
use crate::flight::{self, Join, Table};
use crate::store;
use crate::workbuddy_shared::endpoints::Context;

use super::auth::{self, RefreshedAuth};
use super::region::Region;
use super::session_of_record;

const SAVE_ATTEMPTS: usize = 3;
const SAVE_FAILED_MESSAGE: &str = "WorkBuddy 续期结果未能落盘，请重新登录该账号";
const AUTH_KEYS: &[&str] = &[
    "uid",
    "enterpriseId",
    "type",
    "accessToken",
    "refreshToken",
    "domain",
    "endpoint",
    "prefixPath",
    "platform",
    "edition",
];

fn flight_table() -> &'static Table<String> {
    static TABLE: OnceLock<Table<String>> = OnceLock::new();
    TABLE.get_or_init(Table::new)
}

pub(super) async fn refresh_account(
    ctx: &PluginCtx,
    region: Region,
    account: &str,
    record: &Value,
) -> Result<String, GatewayError> {
    let refresh_token = record
        .get("refreshToken")
        .and_then(Value::as_str)
        .unwrap_or("");
    if refresh_token.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "缺少 refreshToken，无法续期，请重新登录",
        ));
    }
    let key = format!(
        "{}:{account}:{}",
        region.provider_id(),
        flight::fingerprint(refresh_token),
    );
    match flight_table().join(&key) {
        Join::Waiter(waiter) => waiter.wait().await,
        Join::Leader(leader) => {
            let result = refresh_and_save(ctx, region, account, record).await;
            leader.finish(result.clone());
            result
        }
    }
}

async fn refresh_and_save(
    ctx: &PluginCtx,
    region: Region,
    account: &str,
    previous: &Value,
) -> Result<String, GatewayError> {
    let record = store::record(ctx, region.provider_id(), account).await?;
    if !same_auth(previous, &record) {
        return current_token(previous, &record);
    }
    let context = Context::make(
        record.get("endpoint").and_then(Value::as_str),
        record.get("prefixPath").and_then(Value::as_str),
        record.get("platform").and_then(Value::as_str),
        Some(region.edition_id()),
    );
    let proxy: Option<WireProxy> = record
        .get("proxy")
        .and_then(|value| serde_json::from_value(value.clone()).ok());
    let session = session_of_record(&record, region);
    let auth_value = session.get("auth").cloned().unwrap_or(Value::Null);
    let next = match auth::refresh_session(&auth_value, &context, proxy.as_ref()).await {
        Ok(next) => next,
        Err(error) => {
            // 重新登录或另一轮刷新可能在上游请求期间替换凭证。
            if let Ok(latest) = store::record(ctx, region.provider_id(), account).await {
                if !same_auth(&record, &latest) {
                    return current_token(&record, &latest);
                }
            }
            return Err(error);
        }
    };
    save_refresh(ctx, region, account, &record, &next).await
}

async fn save_refresh(
    ctx: &PluginCtx,
    region: Region,
    account: &str,
    previous: &Value,
    next: &RefreshedAuth,
) -> Result<String, GatewayError> {
    let mut current = previous.clone();
    for _ in 0..SAVE_ATTEMPTS {
        if !same_auth(previous, &current) {
            return current_token(previous, &current);
        }
        let expect = store::secret_view(&current);
        let secret = with_refresh(&expect, next);
        if store::save(ctx, account, &expect, &secret).await? {
            crate::logging::log(
                "[Accounts]",
                &format!("✅ {} 账号 token 已刷新（{account}）", region.label()),
            );
            return Ok(next.access_token.clone());
        }
        current = store::record(ctx, region.provider_id(), account).await?;
    }
    if !same_auth(previous, &current) {
        return current_token(previous, &current);
    }
    Err(GatewayError::with_status(409, SAVE_FAILED_MESSAGE))
}

fn same_auth(left: &Value, right: &Value) -> bool {
    AUTH_KEYS
        .iter()
        .all(|key| left.get(*key) == right.get(*key))
}

fn current_token(previous: &Value, record: &Value) -> Result<String, GatewayError> {
    let previous_token = previous.get("accessToken").and_then(Value::as_str);
    record
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty() && Some(*token) != previous_token)
        .map(str::to_string)
        .ok_or_else(|| GatewayError::with_status(409, SAVE_FAILED_MESSAGE))
}

fn with_refresh(secret: &Value, next: &RefreshedAuth) -> Value {
    let mut fields = vec![
        ("accessToken", Value::String(next.access_token.clone())),
        (
            "tokenTail",
            Value::String(store::token_tail(&next.access_token)),
        ),
    ];
    if let Some(token) = next
        .refresh_token
        .as_ref()
        .filter(|token| !token.is_empty())
    {
        fields.push(("refreshToken", Value::String(token.clone())));
    }
    if let Some(value) = next.expires_at.filter(|value| *value > 0.0) {
        fields.push(("expiresAt", serde_json::json!(value)));
    }
    if let Some(value) = next.refresh_expires_at.filter(|value| *value > 0.0) {
        fields.push(("refreshExpiresAt", serde_json::json!(value)));
    }
    store::with_fields(secret, &fields)
}
