//! 国际官网Cookie：原生web_session.rs实际实现，不沿旧keepalive注释发送Bearer建网页会话。
//! 固定www.workbuddy.ai；网页登录码仅内存/账号私有secret，重定向不跨源，不自动接受协议。
use crate::{
    account, endpoints, headers,
    journal::Job,
    network,
    session::{self, Session},
};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

const KEY: &str = "workbuddyWebSession";
const MAX_AGE: i64 = 7 * 86_400_000;
fn valid(cookie: &str) -> bool {
    cookie.starts_with("session=")
        && cookie.len() > 8
        && cookie.len() <= 16_384
        && cookie[8..]
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b';' && b != b',')
}
fn parse_cookie(header: &str) -> Option<(String, i64)> {
    let mut parts = header.split(';');
    let cookie = parts.next()?.trim().to_string();
    if !valid(&cookie) {
        return None;
    }
    let now = sdk::now();
    let mut expires = now.saturating_add(MAX_AGE);
    let mut max_age = None;
    for part in parts {
        let Some((key, value)) = part.trim().split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.to_ascii_lowercase().as_str() {
            "domain"
                if !value
                    .trim_start_matches('.')
                    .eq_ignore_ascii_case("www.workbuddy.ai") =>
            {
                return None
            }
            "path" if value != "/" => return None,
            "max-age" => {
                max_age = Some(
                    now.saturating_add(
                        value
                            .parse::<i64>()
                            .ok()?
                            .clamp(0, MAX_AGE / 1000)
                            .saturating_mul(1000),
                    ),
                )
            }
            "expires" => {
                if let Ok(at) = chrono::DateTime::parse_from_rfc2822(value) {
                    expires = expires.min(at.timestamp_millis());
                }
            }
            _ => {}
        }
    }
    let expires = max_age.unwrap_or(expires);
    (expires > now).then_some((cookie, expires))
}
fn cached(current: &Session, stale: Option<&str>) -> Option<Value> {
    let value = &current.record["secret"][KEY];
    let cookie = value["cookie"].as_str()?;
    (value["origin"] == endpoints::ORIGIN
        && value["authority"] == account::authority(&current.secret)
        && value["expiresAt"]
            .as_i64()
            .is_some_and(|at| at > sdk::now())
        && valid(cookie)
        && stale != Some(cookie))
    .then(|| value.clone())
}
fn ensure(current: &Session, job: &Job, stale: Option<&str>) -> Result<Value> {
    job.check(current)?;
    let latest = session::snapshot()?;
    if let Some(cookie) = cached(&latest, stale) {
        return Ok(cookie);
    }
    let mut h = headers::auth(&latest.secret);
    if let Some(h) = h.as_object_mut() {
        h.remove("X-Department-Info");
    }
    headers::merge(
        &mut h,
        json!({ "Content-Type": "application/json", "Accept": "*/*", "User-Agent": "node",
        "Accept-Language": "*", "Sec-Fetch-Mode": "cors" }),
    );
    if !latest.refresh().is_empty() {
        h["X-Refresh-Token"] = json!(latest.refresh());
    }
    let data = network::raw_timeout(
        "POST",
        endpoints::ORIGIN,
        "/v2/plugin/device/auth/code",
        h,
        None,
        job.remaining_ms()?.min(20_000),
    )?
    .checked(&latest.secrets())?;
    let code = data["deviceCode"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 8192)
        .ok_or_else(|| Error::http(502, "国际站没有签发一次性网页登录码"))?;
    let mut url = url::Url::parse(endpoints::ORIGIN).map_err(|_| Error::new("国际官网地址无效"))?;
    url.set_path("/console/client-login");
    url.query_pairs_mut()
        .append_pair("code", code)
        .append_pair("target", "/profile/growth-center");
    let mut cookie: Option<(String, i64)> = None;
    for _ in 0..6 {
        job.check(current)?;
        if url.origin().ascii_serialization() != endpoints::ORIGIN
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(Error::http(502, "国际官网登录发生跨域跳转，未跟随"));
        }
        let mut h =
            json!({ "Accept": "text/html,application/xhtml+xml", "User-Agent": endpoints::WEB_UA });
        if let Some((cookie, _)) = &cookie {
            h["Cookie"] = json!(cookie);
        }
        let path = format!(
            "{}{}",
            url.path(),
            url.query().map(|q| format!("?{q}")).unwrap_or_default()
        );
        let response = network::open_timeout(
            "GET",
            endpoints::ORIGIN,
            &path,
            h,
            None,
            job.remaining_ms()?.min(20_000),
        )?;
        for header in response.header_values("set-cookie") {
            if let Some(parsed) = parse_cookie(header) {
                cookie = Some(parsed);
            }
        }
        let redirect = (300..400).contains(&response.status);
        if !(200..400).contains(&response.status) && cookie.is_none() {
            return Err(Error::http(response.status.max(400), "国际官网登录被拒绝"));
        }
        let next = response
            .header_values("location")
            .first()
            .and_then(|value| url.join(value).ok());
        drop(response);
        if !redirect {
            break;
        }
        let Some(next) = next else { break };
        url = next;
    }
    let (cookie, expires) =
        cookie.ok_or_else(|| Error::http(502, "国际官网未返回有效session Cookie"))?;
    let saved = json!({ "cookie": cookie, "expiresAt": expires, "authority": account::authority(&latest.secret), "origin": endpoints::ORIGIN });
    for _ in 0..3 {
        job.check(current)?;
        let now = session::snapshot()?;
        if !account::same_auth(&latest.secret, &now.secret) {
            return Err(Error::http(
                409,
                "国际官网登录期间凭据已变化，未保存旧Cookie",
            ));
        }
        let mut secret = now.record["secret"].clone();
        secret[KEY] = saved.clone();
        if account::save(&now.record, &secret)? {
            return Ok(saved);
        }
    }
    Err(Error::http(409, "国际官网Cookie未能保存，未继续未落库状态"))
}
pub fn ready(current: &Session, job: &Job) -> Result<Value> {
    ensure(current, job, None)
}
pub fn renew(current: &Session, job: &Job, stale: &Value) -> Result<Value> {
    ensure(current, job, stale["cookie"].as_str())
}
pub fn request(
    current: &Session,
    job: &Job,
    saved: &Value,
    body: &Value,
) -> Result<network::Reply> {
    job.check(current)?;
    if saved["authority"] != account::authority(&current.secret)
        || saved["origin"] != endpoints::ORIGIN
    {
        return Err(Error::http(409, "国际网页会话归属不符"));
    }
    let cookie = saved["cookie"]
        .as_str()
        .filter(|s| valid(s))
        .ok_or_else(|| Error::http(502, "国际网页Cookie无效"))?;
    let h = json!({ "Cookie": cookie, "Accept": "application/json, text/plain, */*", "User-Agent": endpoints::WEB_UA,
        "Origin": endpoints::ORIGIN, "Referer": format!("{}/profile/growth-center", endpoints::ORIGIN) });
    let mut reply = network::raw_timeout(
        "POST",
        endpoints::ORIGIN,
        "/console/as/conversations/",
        h,
        Some(body),
        job.remaining_ms()?.min(20_000),
    )?;
    if !reply.ok() || !reply.data()["id"].as_str().is_some_and(|id| !id.is_empty()) {
        let message = crate::errors::clean(
            reply.message(),
            &[
                cookie,
                cookie.strip_prefix("session=").unwrap_or(cookie),
                current.token(),
                current.refresh(),
            ],
        );
        reply.value =
            json!({ "code": reply.code(), "message": message, "error": { "message": message } });
    }
    Ok(reply)
}
