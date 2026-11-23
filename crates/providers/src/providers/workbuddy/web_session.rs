//! 官网登录态只绑定当前账号、身份和固定 HTTPS 域，保存在私有凭证中。

use agent2api_provider_protocol::WireProxy;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};
use std::sync::OnceLock;

use super::{catalog_access, contract, region::Region};
use crate::{
    errors::GatewayError,
    http::AccountRequest,
    flight::{Join, Table},
    store,
};

const SECRET_KEY: &str = "workbuddyWebSession";
const MAX_BYTES: usize = 1024 * 1024;
const MAX_AGE_MS: i64 = 7 * 24 * 60 * 60_000;

#[derive(Clone)]
struct SessionCookie {
    cookie: String,
    expires_at: i64,
    authority: Value,
    origin: String,
}

fn cached(record: &Value, session: &Value, origin: &str) -> Option<SessionCookie> {
    let saved = record.get(SECRET_KEY)?;
    let cookie = saved.get("cookie").and_then(Value::as_str)?.to_string();
    let expires_at = saved.get("expiresAt").and_then(Value::as_i64)?;
    if expires_at <= crate::logging::now_ms()
        || saved.get("authority") != Some(&catalog_access::authority(session))
        || saved.get("origin").and_then(Value::as_str) != Some(origin)
        || !valid_cookie(&cookie)
    {
        return None;
    }
    Some(SessionCookie {
        cookie,
        expires_at,
        authority: catalog_access::authority(session),
        origin: origin.into(),
    })
}

async fn ensure(
    ctx: &PluginCtx,
    region: Region,
    id: &str,
    stale: Option<&str>,
) -> Result<SessionCookie, GatewayError> {
    let key = format!("workbuddy-web:{}:{id}", region.id());
    static FLIGHTS: OnceLock<Table<SessionCookie>> = OnceLock::new();
    match FLIGHTS.get_or_init(Table::new).join(&key) {
        Join::Waiter(waiter) => waiter.wait().await,
        Join::Leader(leader) => {
            let result = ensure_and_save(ctx, region, id, stale).await;
            leader.finish(result.clone());
            result
        }
    }
}

async fn ensure_and_save(
    ctx: &PluginCtx,
    region: Region,
    id: &str,
    stale: Option<&str>,
) -> Result<SessionCookie, GatewayError> {
    let record = store::record(ctx, region.provider_id(), id).await?;
    let session = super::session_of_record(&record, region);
    let origin = contract::web_origin(region);
    if let Some(cookie) = cached(&record, &session, origin) {
        if stale.is_none_or(|stale| cookie.cookie != stale) {
            return Ok(cookie);
        }
    }
    let proxy = contract::proxy_of(&record)?;
    let cookie = acquire(&session, region, proxy.as_ref()).await?;
    for _ in 0..3 {
        let latest = store::record(ctx, region.provider_id(), id).await?;
        let current_session = super::session_of_record(&latest, region);
        if catalog_access::authority(&current_session) != cookie.authority
            || latest.get("accessToken") != record.get("accessToken")
            || latest.get("refreshToken") != record.get("refreshToken")
        {
            return Err(GatewayError::with_status(
                409,
                "网页登录期间账号身份或凭证已变化，未保存旧 Cookie",
            ));
        }
        let expect = store::secret_view(&latest);
        let next = store::with_fields(
            &expect,
            &[(
                SECRET_KEY,
                json!({
                    "cookie": cookie.cookie, "expiresAt": cookie.expires_at,
                    "authority": cookie.authority, "origin": cookie.origin,
                }),
            )],
        );
        if store::save(ctx, id, &expect, &next).await? {
            return Ok(cookie);
        }
    }
    Err(GatewayError::with_status(
        502,
        "官网 Cookie 未能保存，未继续使用未落盘的登录态",
    ))
}

async fn acquire(
    session: &Value,
    region: Region,
    proxy: Option<&WireProxy>,
) -> Result<SessionCookie, GatewayError> {
    let origin = contract::web_origin(region);
    let client = strict_client(proxy)?;
    let mut request = client
        .post(format!("{origin}/v2/plugin/device/auth/code"))
        .header("Content-Type", "application/json")
        .header("Accept", "*/*")
        .header("User-Agent", "node")
        .header("Accept-Language", "*")
        .header("Sec-Fetch-Mode", "cors");
    for (key, value) in super::auth::build_auth_headers(session) {
        if !key.eq_ignore_ascii_case("X-Department-Info") {
            request = request.header(key, value);
        }
    }
    if let Some(token) = session
        .pointer("/auth/refreshToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
    {
        request = request.header("X-Refresh-Token", token);
    }
    let response = request
        .with_account_user_agent().send()
        .await
        .map_err(|error| transport_error("获取网页登录码", error))?;
    let status = response.status().as_u16();
    let payload = match bounded_json(response).await {
        Ok(payload) => payload,
        Err(_) if !(200..300).contains(&status) => {
            return Err(GatewayError::with_status(
                i32::from(status).max(400),
                "官方未签发网页登录码，请检查当前登录态",
            ));
        }
        Err(error) => return Err(error),
    };
    let business_code = payload.get("code").and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
    });
    if !(200..300).contains(&status) || business_code.is_some_and(|code| code != 0) {
        return Err(GatewayError::with_status(
            if business_code == Some(12153) {
                401
            } else {
                i32::from(status).max(400)
            },
            "官方未签发网页登录码，请检查当前登录态",
        )
        .with_optional_code(business_code));
    }
    let code = payload
        .pointer("/data/deviceCode")
        .and_then(Value::as_str)
        .filter(|code| !code.is_empty())
        .ok_or_else(|| GatewayError::with_status(502, "官方响应缺少一次性网页登录码"))?;
    let mut url = url::Url::parse(&format!("{origin}/console/client-login"))
        .map_err(|_| GatewayError::new("官网地址配置错误"))?;
    url.query_pairs_mut()
        .append_pair("code", code)
        .append_pair("target", "/profile/growth-center");
    let mut cookie = None;
    let mut expires = crate::logging::now_ms().saturating_add(MAX_AGE_MS);
    for _ in 0..6 {
        if url.origin().ascii_serialization() != origin || url.scheme() != "https" {
            break;
        }
        let mut request = client
            .get(url.clone())
            .header("Accept", "text/html,application/xhtml+xml")
            .header("User-Agent", super::growth::WEB_UA);
        if let Some(cookie) = &cookie {
            request = request.header("Cookie", cookie);
        }
        let response = match request.with_account_user_agent().send().await {
            Ok(response) => response,
            Err(error) if cookie.is_some() => {
                let _ = error;
                break;
            }
            Err(error) => return Err(transport_error("官网登录", error)),
        };
        for header in response.headers().get_all(reqwest::header::SET_COOKIE) {
            if let Ok(text) = header.to_str() {
                if let Some((value, expiry)) = parse_cookie(text, url.host_str().unwrap_or("")) {
                    cookie = Some(value);
                    expires = expiry;
                }
            }
        }
        let status = response.status();
        if !status.is_success() && !status.is_redirection() && cookie.is_none() {
            return Err(GatewayError::with_status(
                i32::from(status.as_u16()),
                "官网 client-login 拒绝了登录码",
            ));
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|location| url.join(location).ok());
        drop(response);
        if !status.is_redirection() {
            break;
        }
        let Some(location) = location else { break };
        url = location;
    }
    let cookie = cookie
        .filter(|cookie| valid_cookie(cookie))
        .ok_or_else(|| GatewayError::with_status(502, "官网没有返回有效 session Cookie"))?;
    Ok(SessionCookie {
        cookie,
        expires_at: expires,
        authority: catalog_access::authority(session),
        origin: origin.into(),
    })
}

pub(super) async fn request(
    ctx: &PluginCtx,
    region: Region,
    id: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
    extra: &[(String, String)],
) -> Result<crate::http::ApiResponse, GatewayError> {
    let origin = contract::web_origin(region);
    let url = url::Url::parse(origin)
        .and_then(|url| url.join(path))
        .map_err(|_| GatewayError::with_status(400, "网页接口路径无效"))?;
    if !path.starts_with('/')
        || path.starts_with("//")
        || url.origin().ascii_serialization() != origin
    {
        return Err(GatewayError::with_status(400, "网页登录态不得跨域使用"));
    }
    let mut cookie = ensure(ctx, region, id, None).await?;
    for attempt in 0..2 {
        let record = store::record(ctx, region.provider_id(), id).await?;
        let session = super::session_of_record(&record, region);
        if catalog_access::authority(&session) != cookie.authority {
            return Err(GatewayError::with_status(409, "网页请求的账号身份已经变化"));
        }
        let proxy = contract::proxy_of(&record)?;
        let client = strict_client(proxy.as_ref())?;
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| GatewayError::with_status(400, "网页请求方法无效"))?;
        let mut request = client
            .request(method, url.clone())
            .header("Cookie", &cookie.cookie)
            .header("Accept", "application/json, text/plain, */*")
            .header("User-Agent", super::growth::WEB_UA)
            .header("Origin", origin)
            .header("Referer", format!("{origin}/profile/growth-center"));
        for (key, value) in extra {
            if [
                "authorization",
                "cookie",
                "host",
                "origin",
                "x-refresh-token",
                "x-user-id",
                "x-enterprise-id",
                "x-tenant-id",
                "x-domain",
            ]
            .iter()
            .any(|name| key.eq_ignore_ascii_case(name))
            {
                continue;
            }
            if key.eq_ignore_ascii_case("referer")
                && !url::Url::parse(value)
                    .ok()
                    .is_some_and(|url| url.origin().ascii_serialization() == origin)
            {
                continue;
            }
            request = request.header(key, value);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .with_account_user_agent().send()
            .await
            .map_err(|error| transport_error("网页接口", error))?;
        let status = response.status().as_u16();
        if status == 401 && attempt == 0 {
            drop(response);
            cookie = ensure(ctx, region, id, Some(&cookie.cookie)).await?;
            continue;
        }
        let payload = bounded_json(response).await?;
        return Ok(crate::http::ApiResponse {
            status,
            ok: (200..300).contains(&status),
            payload: Some(payload),
        });
    }
    Err(GatewayError::with_status(401, "官网登录态被拒绝"))
}

fn strict_client(proxy: Option<&WireProxy>) -> Result<reqwest::Client, GatewayError> {
    let mut builder = reqwest::Client::builder()
        .use_rustls_tls()
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    let proxy = crate::http::proxy_url(proxy, "direct");
    if !proxy.is_empty() && proxy != "direct" {
        let proxy = reqwest::Proxy::all(&proxy)
            .map_err(|_| GatewayError::with_status(502, "网页请求代理地址无效"))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|_| GatewayError::with_status(502, "无法创建网页连接"))
}

async fn bounded_json(mut response: reqwest::Response) -> Result<Value, GatewayError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| transport_error("读取网页响应", error))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_BYTES {
            return Err(GatewayError::with_status(502, "网页响应过大"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| GatewayError::with_status(502, "官网返回了非 JSON 响应"))
}

fn valid_cookie(cookie: &str) -> bool {
    cookie.starts_with("session=")
        && cookie.len() > 8
        && cookie.len() <= 16_384
        && cookie[8..]
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b';' && byte != b',')
}

fn parse_cookie(header: &str, host: &str) -> Option<(String, i64)> {
    let mut parts = header.split(';');
    let cookie = parts.next()?.trim().to_string();
    if !valid_cookie(&cookie) {
        return None;
    }
    let now = crate::logging::now_ms();
    let mut expires = now.saturating_add(MAX_AGE_MS);
    for part in parts {
        let Some((name, value)) = part.trim().split_once('=') else {
            continue;
        };
        match name.to_ascii_lowercase().as_str() {
            "domain" if value.trim_start_matches('.') != host => return None,
            "path" if value != "/" => return None,
            "max-age" => {
                expires = now.saturating_add(
                    value
                        .parse::<i64>()
                        .ok()?
                        .max(0)
                        .min(MAX_AGE_MS / 1000)
                        .saturating_mul(1000),
                )
            }
            "expires" => {
                if let Ok(time) = chrono::DateTime::parse_from_rfc2822(value) {
                    expires = expires.min(time.timestamp_millis());
                }
            }
            _ => {}
        }
    }
    (expires > now).then_some((cookie, expires))
}

fn transport_error(action: &str, error: reqwest::Error) -> GatewayError {
    GatewayError::with_status(
        if error.is_timeout() { 504 } else { 502 },
        format!("{action}失败（网络连接或响应不可用）"),
    )
}
