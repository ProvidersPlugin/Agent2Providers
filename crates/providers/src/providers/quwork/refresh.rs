use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use agent2api_provider_sdk::{PluginCtx, StoreAccount};
use reqwest::{Client, Method};
use serde_json::{json, Value};

use super::{accounts, client, desktop, endpoints};
use crate::errors::GatewayError;

#[derive(Clone)]
struct LocalSession {
    stamp: String,
    secret: Value,
}

fn local_sessions() -> &'static Mutex<HashMap<String, LocalSession>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, LocalSession>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_sessions<T>(work: impl FnOnce(&mut HashMap<String, LocalSession>) -> T) -> T {
    let mut sessions = local_sessions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    work(&mut sessions)
}

fn is_local(record: &StoreAccount) -> bool {
    record.secret.get("source").and_then(Value::as_str) == Some("desktop")
}

fn disk_stamp(secret: &Value) -> String {
    crate::flight::fingerprint(&format!(
        "{}\0{}\0{:?}\0{:?}",
        accounts::text(secret, &["accessToken"]),
        accounts::text(secret, &["llmToken"]),
        secret.get("expiresAt"),
        secret.get("updatedAt")
    ))
}

async fn read(
    ctx: &PluginCtx,
    account: &str,
) -> Result<(StoreAccount, Value, String), GatewayError> {
    let record = ctx
        .store_get(endpoints::PROVIDER, account)
        .await
        .map_err(|_| GatewayError::with_status(404, "找不到 QuWork 账号"))?;
    if !is_local(&record) {
        return Ok((record.clone(), record.secret, String::new()));
    }
    let disk = desktop::current(Some(&record.secret)).await?;
    let stamp = disk_stamp(&disk);
    let secret = with_sessions(|sessions| {
        sessions
            .get(&record.id)
            .filter(|entry| entry.stamp == stamp)
            .map(|entry| entry.secret.clone())
    })
    .unwrap_or(disk);
    Ok((record, secret, stamp))
}

pub async fn snapshot(ctx: &PluginCtx, account: &str) -> Result<Value, GatewayError> {
    let (_, secret, _) = read(ctx, account).await?;
    Ok(secret)
}

pub fn expiring(secret: &Value) -> bool {
    !accounts::text(secret, &["accessToken"]).is_empty()
        && client::timestamp(secret.get("expiresAt"))
            .is_some_and(|expiry| expiry <= crate::http::now_ms() + endpoints::REFRESH_WINDOW_MS)
}

pub async fn refresh(ctx: &PluginCtx, account: &str, force: bool) -> Result<Value, GatewayError> {
    let before = read(ctx, account).await?;
    if !force && !expiring(&before.1) {
        return Ok(before.1);
    }
    let seen = accounts::text(&before.1, &["accessToken"]);
    let key = format!("quwork-refresh:{}", before.0.id);
    let result = crate::flight::run(&key, || async {
        let result = async {
            let (record, mut secret, stamp) = read(ctx, account).await?;
            let access = accounts::text(&secret, &["accessToken"]);
            if access.is_empty() {
                return Err(GatewayError::with_status(
                    400,
                    "QuWork 只提供了模型凭证，不能续期登录会话；请重新导入完整登录态",
                ));
            }
            if access != seen || (!force && !expiring(&secret)) {
                return Ok(secret);
            }
            let reply = client::request(
                &client::client(ctx, None),
                Method::GET,
                endpoints::REFRESH_PATH,
                &access,
                None,
            )
            .await?;
            let data = reply.data("登录续期")?;
            let next = accounts::token(&accounts::text(&data, &["token"]), "登录凭证")?;
            if next.is_empty() {
                return Err(GatewayError::with_status(502, "QuWork 续期未返回登录凭证"));
            }
            let expiry = client::timestamp(data.get("expire"))
                .or_else(|| accounts::jwt_expiry(&next))
                .unwrap_or_else(|| crate::http::now_ms().saturating_add(2 * 60 * 60_000));
            let previous_expiry = client::timestamp(secret.get("expiresAt")).unwrap_or(0);
            if next == access && expiry <= previous_expiry {
                return Err(GatewayError::with_status(
                    502,
                    "QuWork 续期未更新凭证或有效期，请重新登录",
                ));
            }
            secret["accessToken"] = json!(next);
            secret["expiresAt"] = json!(expiry);
            secret["updatedAt"] = json!(crate::http::now_ms());
            // 1.6.0 的续期只更换账号令牌；没有新的 llm_token 时保留原模型凭证。
            let llm = accounts::token(&accounts::text(&data, &["llm_token"]), "模型凭证")?;
            if !llm.is_empty() {
                secret["llmToken"] = json!(llm);
            }
            let mut saved = if is_local(&record) {
                record.secret.clone()
            } else {
                secret.clone()
            };
            if is_local(&record) {
                let current = desktop::current(Some(&record.secret)).await?;
                let current_stamp = disk_stamp(&current);
                if current_stamp != stamp {
                    return Ok(current);
                }
                saved["expiresAt"] = json!(expiry);
                saved["updatedAt"] = json!(crate::http::now_ms());
                with_sessions(|sessions| {
                    if sessions.len() >= 128 {
                        sessions.clear();
                    }
                    sessions.insert(
                        record.id.clone(),
                        LocalSession {
                            stamp,
                            secret: secret.clone(),
                        },
                    );
                });
            }
            let updated = ctx
                .store_update(&record.id, &record.secret, &saved)
                .await
                .map_err(|_| GatewayError::with_status(500, "QuWork 新登录态保存失败"))?;
            if !updated {
                return snapshot(ctx, &record.id).await;
            }
            Ok(secret)
        }
        .await;
        Ok(result)
    })
    .await;
    result.map_err(GatewayError::from)?
}

pub async fn model_token(ctx: &PluginCtx, account: &str) -> Result<String, String> {
    let secret = snapshot(ctx, account)
        .await
        .map_err(GatewayError::into_control_message)?;
    let secret = if expiring(&secret) {
        // 账号服务不可用不应阻断仍有效的独立模型凭证。
        refresh(ctx, account, false).await.unwrap_or(secret)
    } else {
        secret
    };
    let token = accounts::text(&secret, &["llmToken"]);
    if token.is_empty() {
        Err("QuWork 缺少模型凭证，请重新导入账号".into())
    } else {
        Ok(token)
    }
}

pub async fn protected(
    ctx: &PluginCtx,
    account: &str,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<client::Reply, GatewayError> {
    let secret = snapshot(ctx, account).await?;
    let secret = if expiring(&secret) {
        refresh(ctx, account, false).await?
    } else {
        secret
    };
    let access = accounts::text(&secret, &["accessToken"]);
    if access.is_empty() {
        return Err(crate::errors::usage_not_configured(
            "QuWork",
            "登录凭证 accessToken；请重新导入完整登录态",
        ));
    }
    let client: Client = client::client(ctx, None);
    let reply = client::request(&client, method.clone(), path, &access, body).await?;
    if !reply.unauthorized() {
        return Ok(reply);
    }
    let refreshed = refresh(ctx, account, true).await?;
    client::request(
        &client,
        method,
        path,
        &accounts::text(&refreshed, &["accessToken"]),
        body,
    )
    .await
}
