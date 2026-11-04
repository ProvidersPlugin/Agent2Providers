//! Kiro 凭证续期：社交登录 / AWS OIDC / 外部 IdP，以及首次补 profile。
//!
//! ── 刷新链路（与参考实现逐条对应）──────────────────────────
//! ```text
//!   social        POST https://prod.{region}.auth.desktop.kiro.dev/refreshToken
//!                 {"refreshToken": "…"}  → {accessToken, refreshToken, expiresIn}
//!   idc           POST https://oidc.{region}.amazonaws.com/token
//!                 {clientId, clientSecret, refreshToken, grantType} → 同上
//!   external-idp  POST {token_endpoint}（form）→ {access_token, refresh_token, expires_in}
//!   apikey        没有续期接口
//!   本机登录态    先让 kiro-cli 自己续期，再读回；没有 kiro-cli 时走上面的刷新并写回
//! ```
//!
//! ── 为什么用单飞锁 ──────────────────────────────────────────
//! refresh_token 通常是一次性的：两个并发请求同时刷新，后到的那个会用已经
//! 作废的 refresh_token，把好凭证覆盖成坏的。`flight::run` 按账号串行，
//! 拿到锁后**重新读账号**，前一个已经写好的结果直接复用。

use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::credentials::{now_ms, Credentials};
use super::{endpoints, local, PROVIDER_ID};
use crate::{flight, http, logging};

const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 一次刷新得到的 token 三件套。
struct Refreshed {
    access: String,
    refresh: String,
    expires_in: i64,
}

async fn post_json(
    client: &reqwest::Client,
    url: &str,
    body: &Value,
    headers: &[(String, String)],
) -> Result<Value, String> {
    let mut request = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .timeout(std::time::Duration::from_millis(REQUEST_TIMEOUT_MS))
        .json(body);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("刷新 Kiro 登录态时网络失败：{}", http::describe(&error)))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        if matches!(status, 400 | 401 | 403) {
            return Err("Kiro 的登录态已失效，请重新登录 Kiro".to_string());
        }
        return Err(format!("刷新 Kiro 登录态失败（HTTP {status}）"));
    }
    serde_json::from_str(&text).map_err(|_| "刷新 Kiro 登录态的返回不是 JSON".to_string())
}

async fn refresh_once(
    client: &reqwest::Client,
    credentials: &Credentials,
) -> Result<Refreshed, String> {
    if credentials.refresh.is_empty() {
        return Err("Kiro 的登录态已失效，请重新登录 Kiro".to_string());
    }
    let region = credentials.region();
    match credentials.method.as_str() {
        "social" => {
            let body = json!({ "refreshToken": credentials.refresh });
            let headers = vec![("User-Agent".to_string(), endpoints::DESKTOP_UA.to_string())];
            let value =
                post_json(client, &endpoints::refresh_url(&region), &body, &headers).await?;
            Ok(Refreshed {
                access: value
                    .get("accessToken")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                refresh: value
                    .get("refreshToken")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                expires_in: value.get("expiresIn").and_then(Value::as_i64).unwrap_or(0),
            })
        }
        "idc" => {
            let body = json!({
                "clientId": credentials.client_id,
                "clientSecret": credentials.client_secret,
                "refreshToken": credentials.refresh,
                "grantType": "refresh_token",
            });
            let url = format!("{}/token", endpoints::oidc(&region));
            let value = post_json(client, &url, &body, &[]).await?;
            Ok(Refreshed {
                access: value
                    .get("accessToken")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                refresh: value
                    .get("refreshToken")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                expires_in: value.get("expiresIn").and_then(Value::as_i64).unwrap_or(0),
            })
        }
        "external-idp" => {
            if credentials.token_url.is_empty() {
                return Err("Kiro 的登录态已失效，请用 kiro-cli 重新登录".to_string());
            }
            let form = [
                ("grant_type", "refresh_token".to_string()),
                ("client_id", credentials.client_id.clone()),
                ("refresh_token", credentials.refresh.clone()),
            ];
            let response = client
                .post(&credentials.token_url)
                .form(&form)
                .timeout(std::time::Duration::from_millis(REQUEST_TIMEOUT_MS))
                .send()
                .await
                .map_err(|error| {
                    format!("刷新 Kiro 登录态时网络失败：{}", http::describe(&error))
                })?;
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            if !(200..300).contains(&status) {
                return Err("Kiro 的登录态已失效，请用 kiro-cli 重新登录".to_string());
            }
            let value: Value = serde_json::from_str(&text)
                .map_err(|_| "刷新 Kiro 登录态的返回不是 JSON".to_string())?;
            Ok(Refreshed {
                access: value
                    .get("access_token")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                refresh: value
                    .get("refresh_token")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                expires_in: value.get("expires_in").and_then(Value::as_i64).unwrap_or(0),
            })
        }
        _ => Err("Kiro 的登录态已失效，请重新登录 Kiro".to_string()),
    }
}

/// 把一次刷新落到一份凭证上（就地）。
fn apply(credentials: &mut Credentials, refreshed: Refreshed) -> Result<(), String> {
    if refreshed.access.is_empty() {
        return Err("Kiro 的登录态已失效，请重新登录 Kiro".to_string());
    }
    credentials.access = refreshed.access;
    if !refreshed.refresh.is_empty() {
        credentials.refresh = refreshed.refresh;
    }
    let seconds = if refreshed.expires_in > 0 {
        refreshed.expires_in
    } else {
        3600
    };
    credentials.expires = now_ms() + seconds * 1000;
    Ok(())
}

/// 刷新一份凭证：本机来源先让 kiro-cli 自己续期，否则走网络刷新。
async fn refresh_credentials(
    credentials: &Credentials,
    gateway_proxy: &str,
) -> Result<Credentials, String> {
    if credentials.is_api_key() {
        return Err("Kiro 拒绝了保存的 API key，请重新填写".to_string());
    }
    if credentials.db_key.starts_with("kirocli:") {
        if let Some(next) = local::refresh_with_cli().await {
            if next.access != credentials.access
                && next.db_key == credentials.db_key
                && next.fresh(endpoints::EARLY_MS)
            {
                return Ok(next);
            }
        }
    }
    let client = http::control_client(None, gateway_proxy);
    let mut next = credentials.clone();
    let refreshed = refresh_once(&client, &next).await?;
    apply(&mut next, refreshed)?;
    local::save_back(&next);
    Ok(next)
}

/// 在阻塞线程池里读本机登录态（SQLite / 小 JSON，不要让它们卡住事件循环）。
pub async fn read_local(source: &str) -> Option<Credentials> {
    let source = source.to_string();
    tokio::task::spawn_blocking(move || local::read_source(&source))
        .await
        .ok()
        .flatten()
}

/// 读账号并把凭证准备到「现在能用」。
///
/// 账号为空串时用该 provider 的队首账号（网关侧语义）。
pub async fn ensure(ctx: &PluginCtx, account: &str) -> Result<(String, Credentials), String> {
    let stored = ctx
        .store_get(PROVIDER_ID, account)
        .await
        .map_err(|error| format!("读取 Kiro 账号失败：{error}"))?;
    let account_id = stored.id.clone();
    let mut credentials = Credentials::from_secret(&stored.secret);
    if credentials.is_local() {
        let live = read_local(&credentials.source).await.ok_or_else(|| {
            "Kiro 的本机登录态不存在，请先在 kiro-cli 或 Kiro IDE 里登录".to_string()
        })?;
        let mut live = if live.fresh(endpoints::EARLY_MS) {
            live
        } else {
            refresh_credentials(&live, &ctx.env.proxy).await?
        };
        if live.profile.is_empty() {
            let region = live.region();
            if let Ok(profile) = lookup_profile(&live, &region, &ctx.env.proxy).await {
                live.profile = profile;
            }
        }
        return Ok((account_id, live));
    }
    if !credentials.fresh(endpoints::EARLY_MS) {
        credentials = guarded_refresh(ctx, &stored.id, &stored.secret).await?;
    }
    // OAuth 登录态第一次用时补 profile（额度、目录与对话都要它）。API key
    // 与实时读取的本机登录态不走这里：前者不需要 profile，后者在各自的
    // 读取路径里补。
    if !credentials.is_api_key() && !credentials.is_local() && credentials.profile.is_empty() {
        if let Ok(profile) =
            lookup_profile(&credentials, &credentials.region(), &ctx.env.proxy).await
        {
            if !profile.is_empty() {
                credentials.profile = profile;
                let mut secret = stored.secret.clone();
                credentials.write_into(&mut secret);
                let _ = ctx.store_update(&stored.id, &stored.secret, &secret).await;
            }
        }
    }
    Ok((account_id, credentials))
}

/// 401/403 之后的强制刷新（不看临期窗口）。
pub async fn force_refresh(
    ctx: &PluginCtx,
    account: &str,
) -> Result<(String, Credentials), String> {
    let stored = ctx
        .store_get(PROVIDER_ID, account)
        .await
        .map_err(|error| format!("读取 Kiro 账号失败：{error}"))?;
    let credentials = Credentials::from_secret(&stored.secret);
    if credentials.is_local() {
        let live = read_local(&credentials.source).await.ok_or_else(|| {
            "Kiro 的本机登录态不存在，请先在 kiro-cli 或 Kiro IDE 里登录".to_string()
        })?;
        return Ok((
            stored.id.clone(),
            refresh_credentials(&live, &ctx.env.proxy).await?,
        ));
    }
    if credentials.is_api_key() {
        return Err("Kiro 拒绝了保存的 API key，请重新填写".to_string());
    }
    Ok((
        stored.id.clone(),
        guarded_refresh(ctx, &stored.id, &stored.secret).await?,
    ))
}

/// 单飞刷新：同一账号的并发刷新只打一次上游，拿到锁后重读账号。
async fn guarded_refresh(
    ctx: &PluginCtx,
    account_id: &str,
    original: &Value,
) -> Result<Credentials, String> {
    let key = format!("kiro:refresh:{account_id}");
    let account = account_id.to_string();
    let expect = original.clone();
    let ctx = ctx.clone();
    let refreshed = flight::run(&key, || async move {
        if let Ok(current) = ctx.store_get(PROVIDER_ID, &account).await {
            let latest = Credentials::from_secret(&current.secret);
            if latest.access != Credentials::from_secret(&expect).access
                && latest.fresh(endpoints::EARLY_MS)
            {
                return Ok(latest);
            }
        }
        let base = Credentials::from_secret(&expect);
        let mut next = base.clone();
        let client = http::control_client(None, &ctx.env.proxy);
        let refreshed = refresh_once(&client, &base).await?;
        apply(&mut next, refreshed)?;
        let mut secret = expect.clone();
        next.write_into(&mut secret);
        if let Err(error) = ctx.store_update(&account, &expect, &secret).await {
            logging::warn("kiro", &format!("保存刷新后的凭证失败：{error}"));
        }
        Ok(next)
    })
    .await?;
    Ok(refreshed)
}

/// 首次为凭证补 profile（API key 走 GetProfile，其余走 List-Available-Profiles）。
pub async fn ensure_profile(
    ctx: &PluginCtx,
    account_id: &str,
    secret: &Value,
    credentials: &mut Credentials,
) -> Result<(), String> {
    if credentials.is_local() || credentials.is_api_key() {
        return Ok(());
    }
    if !credentials.profile.is_empty() {
        return Ok(());
    }
    let region = credentials.region();
    let profile = match lookup_profile(credentials, &region, &ctx.env.proxy).await {
        Ok(profile) => profile,
        Err(_) => return Ok(()),
    };
    if profile.is_empty() {
        return Ok(());
    }
    credentials.profile = profile;
    let mut next = secret.clone();
    credentials.write_into(&mut next);
    if let Err(error) = ctx.store_update(account_id, secret, &next).await {
        logging::warn("kiro", &format!("保存 profile 失败：{error}"));
    }
    Ok(())
}

/// 找一个登录态用的 profile。
pub async fn lookup_profile(
    credentials: &Credentials,
    region: &str,
    gateway_proxy: &str,
) -> Result<String, String> {
    if credentials.login_provider == "builderid" {
        return Ok(endpoints::BUILDER_ID_PROFILE.to_string());
    }
    let client = http::control_client(None, gateway_proxy);
    let mut last: Option<String> = None;
    for attempt_region in [region, "us-east-1", "eu-central-1"] {
        let url = format!(
            "{}/List-Available-Profiles?origin=KIRO_CLI",
            endpoints::management(attempt_region)
        );
        let mut request = client
            .post(&url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .timeout(std::time::Duration::from_millis(20_000))
            .json(&json!({}));
        request = request.header("Authorization", format!("Bearer {}", credentials.access));
        if let Some(kind) = credentials.token_type() {
            request = request.header("tokentype", kind);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                last = Some(http::describe(&error));
                continue;
            }
        };
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            if credentials.method == "idc" && status == 403 && text.contains("Builder ID") {
                return Ok(endpoints::BUILDER_ID_PROFILE.to_string());
            }
            last = Some(format!("HTTP {status}"));
            continue;
        }
        let value: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if let Some(arn) = value
            .get("profiles")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find_map(|profile| profile.get("arn").and_then(Value::as_str))
        {
            return Ok(arn.to_string());
        }
    }
    Err(last.unwrap_or_else(|| "Kiro 没有为这个登录态提供 profile".to_string()))
}
