use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};

use agent2api_provider_sdk::PluginCtx;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::errors::GatewayError;
use super::{credentials, oauth, subscription};
use credentials::OfficeAceCredential;

async fn refresh_lock(account_id: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Weak<Mutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new())).lock().await;
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(account_id).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(account_id.to_string(), Arc::downgrade(&lock));
    lock
}

async fn read(
    ctx: &PluginCtx,
    account_id: &str,
) -> Result<(Value, OfficeAceCredential), GatewayError> {
    let (id, secret) = crate::store::secret(ctx, super::ID, account_id).await?;
    let mut credential = credentials::from_record(Some(&secret))?;
    credential.id = id;
    Ok((secret, credential))
}

fn can_refresh(secret: &Value) -> bool {
    !crate::store::pick(secret, &["refreshToken", "refresh_token"]).is_empty()
        && secret.get("dpopKeyPair").is_some_and(Value::is_object)
}

pub fn credentials_expiring(record: &Value) -> bool {
    can_refresh(record)
        && credentials::from_record(Some(record))
            .is_ok_and(|credential| credential.control_plane_expiring())
}

pub async fn credential_of(
    ctx: &PluginCtx,
    account_id: &str,
) -> Result<OfficeAceCredential, GatewayError> {
    let (secret, credential) = read(ctx, account_id).await?;
    if credential.control_plane_expiring() && can_refresh(&secret) {
        refresh(ctx, &credential.id, Some(&credential)).await
    } else {
        Ok(credential)
    }
}

fn refresh_token_of(secret: &Value) -> String {
    crate::store::pick(secret, &["refreshToken", "refresh_token"])
}

fn same_control(left: &OfficeAceCredential, right: &OfficeAceCredential) -> bool {
    left.access_key_id == right.access_key_id
        && left.secret_access_key == right.secret_access_key
        && left.security_token == right.security_token
        && left.expires_at == right.expires_at
}

pub async fn refresh(
    ctx: &PluginCtx,
    account_id: &str,
    rejected: Option<&OfficeAceCredential>,
) -> Result<OfficeAceCredential, GatewayError> {
    let (before_secret, before) = read(ctx, account_id).await?;
    let lock = refresh_lock(&before.id).await;
    // refresh token 一次性轮换：发请求前串行并回读，比较再写无法阻止上游重复消费。
    let _guard = lock.lock().await;
    let (secret, current) = read(ctx, &before.id).await?;
    if !same_control(rejected.unwrap_or(&before), &current)
        || refresh_token_of(&before_secret) != refresh_token_of(&secret)
    {
        return Ok(current);
    }
    let refresh_token = refresh_token_of(&secret);
    if refresh_token.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "OfficeAce 账号没有 refresh token（手工只导入了网关凭据）：\
             控制面凭据无法续期，额度/签到需要重新登录一次",
        ));
    }
    let dpop = secret
        .get("dpopKeyPair")
        .filter(|value| value.is_object())
        .ok_or_else(|| GatewayError::with_status(
            400,
            "OfficeAce 账号缺少 DPoP 私钥上下文，无法续期：请重新登录一次",
        ))?;
    let (renewed, expires_at, _user_name, next_refresh) = oauth::refresh(dpop, &refresh_token)
        .await
        .map_err(|reason| GatewayError::with_status(401, format!("OfficeAce 续期失败：{reason}")))?;
    let project_id = if renewed.project_id.is_empty() {
        current.project_id.clone()
    } else {
        renewed.project_id
    };
    let next = crate::store::with_fields(
        &secret,
        &[
            ("accessKeyId", Value::String(renewed.access_key_id)),
            ("secretAccessKey", Value::String(renewed.secret_access_key)),
            ("securityToken", Value::String(renewed.security_token)),
            ("projectId", Value::String(project_id)),
            ("expiresAt", Value::from(expires_at)),
            ("refreshToken", Value::String(next_refresh.clone())),
        ],
    );
    // expect 必须是原始 secret，不能剥掉其中真实存在的 name 或其他凭证字段。
    let saved = crate::store::save(ctx, &before.id, &secret, &next).await?;
    let (latest_secret, latest) = read(ctx, &before.id).await?;
    let latest_refresh = refresh_token_of(&latest_secret);
    let expected = credentials::from_record(Some(&next))?;
    if latest_refresh == next_refresh && same_control(&expected, &latest) {
        return Ok(latest);
    }
    if !saved && !latest_refresh.is_empty() && latest_refresh != refresh_token
        && !same_control(&current, &latest)
    {
        return Ok(latest);
    }
    Err(GatewayError::with_status(
        409,
        "OfficeAce 续期结果未能完整落盘，账号凭据已发生变化：请重新登录一次",
    ))
}

pub async fn claim_bonus(
    ctx: &PluginCtx,
    credential: &mut OfficeAceCredential,
) -> Result<Value, GatewayError> {
    match subscription::claim_bonus(credential).await {
        Err(error) if error.code.as_deref() == Some(subscription::CONTROL_TOKEN_EXPIRED) => {
            *credential = refresh(ctx, &credential.id, Some(credential)).await?;
            subscription::claim_bonus(credential).await
        }
        result => result,
    }
}
