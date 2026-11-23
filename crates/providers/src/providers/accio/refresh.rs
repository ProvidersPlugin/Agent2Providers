//! Accio 凭证续期：同一凭证只刷一次，保存时确认账号未被重新导入或删除。
//!
//! 上游：`POST /api/auth/refresh_token`，body `{accessToken, refreshToken}`
//! → `{accessToken, refreshToken, expiresAt}`（桌面端同名链路，见
//! `core::login` 里那套账号切换的安全版是 `/api/auth/safe/refresh_token`，
//! 本网关不做账号切换，走普通那条）。
//!
//! 上游可能保持令牌原值、只延长有效期；不能仅以令牌是否变化判定续期成功。
//! 未返回有效期时使用 1 小时作为下次主动刷新时间。

use std::sync::OnceLock;

use serde_json::Value;

use agent2api_provider_sdk::PluginCtx;

use crate::flight::{Join, Table};
use crate::errors::GatewayError;
use crate::logging;

use super::auth;
use super::credentials::{self, Credentials};
use super::endpoints;

/// 上游没给到期时间时的保守默认：1 小时
const DEFAULT_TTL_MS: i64 = 60 * 60 * 1000;

static FLIGHTS: OnceLock<Table<Credentials>> = OnceLock::new();

/// 取账号记录 + 凭证（不刷新）。
///
/// `region` 必给：两个地区是两家 provider，取记录时按它过滤（`store::record`
/// 已经按 provider 过滤，这里再核一次地区 —— 记录里的 `mode` 与 provider
/// 不一致时说明这条账号是从另一家迁过来的脏数据）。
pub async fn snapshot(
    ctx: &PluginCtx,
    account_id: &str,
    region: super::endpoints::Region,
) -> Result<(Value, Credentials), GatewayError> {
    let record = crate::store::record(ctx, region.provider_id(), account_id).await?;
    if !record.is_object() {
        return Err(GatewayError::with_status(404, "Accio 账号不存在，请先添加账号"));
    }
    let credentials = auth::snapshot(&record)?;
    Ok((record, credentials))
}

/// 确保拿到可用凭证（`force = true` 时不看临期窗口，401 之后强制刷一次）。
pub async fn ensure_fresh(
    ctx: &PluginCtx,
    account_id: &str,
    region: super::endpoints::Region,
    force: bool,
) -> Result<Credentials, GatewayError> {
    let (record, credentials) = snapshot(ctx, account_id, region).await?;
    if !force && !credentials.expiring() {
        return Ok(credentials);
    }
    if !credentials.can_refresh() {
        return Err(GatewayError::with_status(
            400,
            "Accio 账号没有 refreshToken，无法续期，请重新登录或粘贴完整凭证",
        ));
    }
    // 单飞键：插件只有一个进程、一个账号库，所以不需要网关那份 `file_string()`
    // 前缀（那是为了区分同一进程里挂多个库的情形）。
    let key = format!(
        "accio:{}:{}:{}:{}",
        account_id,
        credentials.region.edition(),
        crate::flight::fingerprint(&credentials.access_token),
        crate::flight::fingerprint(&credentials.refresh_token),
    );
    match FLIGHTS.get_or_init(Table::new).join(&key) {
        Join::Waiter(waiter) => waiter.wait().await,
        Join::Leader(leader) => {
            let result = refresh_and_save(ctx, &record, &credentials).await;
            leader.finish(result.clone());
            result
        }
    }
}

pub async fn after_rejection(
    ctx: &PluginCtx,
    account_id: &str,
    region: super::endpoints::Region,
    rejected: &Credentials,
) -> Result<Credentials, GatewayError> {
    let (_, current) = snapshot(ctx, account_id, region).await?;
    // 在途请求被拒时，其他请求可能已经完成续期，不能再次消耗旧刷新凭证。
    if current.access_token != rejected.access_token
        || current.refresh_token != rejected.refresh_token
        || current.expires_at > rejected.expires_at
    {
        return Ok(current);
    }
    ensure_fresh(ctx, account_id, region, true).await
}

async fn refresh_and_save(
    ctx: &PluginCtx,
    record: &Value,
    credentials: &Credentials,
) -> Result<Credentials, GatewayError> {
    let proxy = auth::account_proxy(record)?;
    let body = serde_json::json!({
        "accessToken": credentials.access_token,
        "refreshToken": credentials.refresh_token,
    });
    let response = auth::post_json(credentials.region, endpoints::REFRESH_TOKEN_PATH, &body, proxy.as_ref())
        .await?;
    let data = auth::payload(response, "凭证续期").map(auth::unwrap_data)?;
    let token = credentials::secret(&data, &["accessToken", "access_token", "token"])?;
    if token.is_empty() {
        return Err(GatewayError::with_status(502, "Accio 续期响应缺少 accessToken，旧凭证未被覆盖"));
    }
    let mut fresh = credentials.clone();
    fresh.access_token = token;
    let refresh = credentials::secret(&data, &["refreshToken", "refresh_token"])?;
    if !refresh.is_empty() {
        fresh.refresh_token = refresh;
    }
    // 续期响应带回新的 sgcookie 就换掉；没带就沿用旧的（`clone` 已经带上）
    let sgcookie = credentials::sgcookie(&data);
    if !sgcookie.is_empty() {
        fresh.sgcookie = sgcookie;
    }
    fresh.expires_at = Some(
        credentials::timestamp(
            data.get("expiresAt").or_else(|| data.get("expires_at")),
        )
        .unwrap_or_else(|| logging::now_ms() + DEFAULT_TTL_MS),
    );
    // 身份/地区不允许被续期结果改掉：那意味着串号（换到了另一个账号的凭证）
    if !credentials.user_id.is_empty() {
        fresh.user_id = credentials.user_id.clone();
    }
    fresh.device_id = credentials.device_id.clone();
    // 比较再写：`expect` 是读到时的 secret，网关只在它没变时才写。
    // 与网关那份 `update_accio_credentials_if_current` 的「逐键比对 5 个字段」
    // 同一语义（`store::save` 的通用形态）。
    let expect = crate::store::secret_view(record);
    let next = merge_credentials(&expect, &fresh);
    let id = record.get("id").and_then(Value::as_str).unwrap_or("");
    if id.is_empty() {
        return Err(GatewayError::with_status(
            409,
            "Accio 续期结果无法写回（账号记录缺 id），请重新添加该账号",
        ));
    }
    if crate::store::save(ctx, id, &expect, &next).await? {
        return Ok(fresh);
    }
    // 回写被拒（记录已被换掉）：用最新那份，别把旧结果当成功用出去
    snapshot(ctx, id, credentials.region)
        .await
        .map(|(_, credentials)| credentials)
}

/// 把续期结果并进既有 secret（空值不覆盖）。
///
/// 与网关 `update_accio_credentials_if_current` 的「只并非空字段」逐条对齐：
/// 重新登录时只给了一个 accessToken，不该把上次的 refreshToken / expiresAt
/// 洗掉。
fn merge_credentials(expect: &Value, fresh: &Credentials) -> Value {
    // 逐键显式列出（与 `Credentials::to_value` 的键集合一致；空值不覆盖）
    let mut fields: Vec<(&str, Value)> = Vec::new();
    for (key, value) in [
        ("accessToken", Value::String(fresh.access_token.clone())),
        ("refreshToken", Value::String(fresh.refresh_token.clone())),
        ("userId", Value::String(fresh.user_id.clone())),
        ("email", Value::String(fresh.email.clone())),
        ("nickname", Value::String(fresh.name.clone())),
        ("deviceId", Value::String(fresh.device_id.clone())),
        ("sgcookie", Value::String(fresh.sgcookie.clone())),
        ("mode", Value::String(fresh.region.edition().to_string())),
    ] {
        if value.is_null() || matches!(&value, Value::String(text) if text.is_empty()) {
            continue;
        }
        fields.push((key, value));
    }
    if let Some(expires_at) = fresh.expires_at {
        fields.push(("expiresAt", Value::from(expires_at)));
    }
    fields.push((
        "tokenTail",
        Value::String(crate::store::token_tail(&fresh.access_token)),
    ));
    crate::store::with_fields(expect, &fields)
}
