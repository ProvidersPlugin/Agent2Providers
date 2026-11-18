//! 官网Cookie仅绑定当前账号与固定国内来源，普通账号secret持久化，绝不公开。
//! device/auth/code→同源client-login重定向最多6跳；后续web请求只用Cookie不携Bearer。
use crate::{
    account, endpoints, headers, network,
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
    let mut until = now.saturating_add(MAX_AGE);
    let mut max_age = None;
    for part in parts {
        if let Some((key, value)) = part.trim().split_once('=') {
            let value = value.trim();
            match key.to_ascii_lowercase().as_str() {
                "domain"
                    if !value
                        .trim_start_matches('.')
                        .eq_ignore_ascii_case("www.workbuddy.cn") =>
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
                    if let Ok(value) = chrono::DateTime::parse_from_rfc2822(value) {
                        until = until.min(value.timestamp_millis());
                    }
                }
                _ => {}
            }
        }
    }
    let until = max_age.unwrap_or(until);
    (until > now).then_some((cookie, until))
}
fn cached(current: &Session, stale: Option<&str>) -> Option<Value> {
    let saved = &current.record["secret"][KEY];
    let cookie = saved["cookie"].as_str()?;
    (saved["origin"] == endpoints::ORIGIN
        && saved["authority"] == account::authority(&current.secret)
        && saved["expiresAt"]
            .as_i64()
            .is_some_and(|at| at > sdk::now())
        && valid(cookie)
        && stale != Some(cookie))
    .then(|| saved.clone())
}
fn ensure(current: &Session, stale: Option<&str>) -> Result<Value> {
    let latest = session::snapshot()?;
    if account::authority(&latest.secret) != account::authority(&current.secret) {
        return Err(Error::http(409, "官网登录期间账号身份已变化"));
    }
    if let Some(cookie) = cached(&latest, stale) {
        return Ok(cookie);
    }
    let mut request_headers = headers::auth(&latest.secret);
    request_headers
        .as_object_mut()
        .map(|v| v.remove("X-Department-Info"));
    headers::merge(
        &mut request_headers,
        json!({ "Content-Type": "application/json", "Accept": "*/*", "User-Agent": "node", "Accept-Language": "*", "Sec-Fetch-Mode": "cors" }),
    );
    if !latest.refresh().is_empty() {
        request_headers["X-Refresh-Token"] = json!(latest.refresh());
    }
    let data = network::request(
        "POST",
        endpoints::ORIGIN,
        "/v2/plugin/device/auth/code",
        request_headers,
        None,
        &latest.secrets(),
    )?;
    let code = data["deviceCode"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 8192)
        .ok_or_else(|| Error::http(502, "官方没有签发网页登录码"))?;
    let mut url = url::Url::parse(endpoints::ORIGIN).map_err(|_| Error::new("官网地址无效"))?;
    url.set_path("/console/client-login");
    url.query_pairs_mut()
        .append_pair("code", code)
        .append_pair("target", "/profile/growth-center");
    let mut cookie: Option<(String, i64)> = None;
    for _ in 0..6 {
        if url.origin().ascii_serialization() != endpoints::ORIGIN
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(Error::http(502, "官网登录发生跨域跳转，未跟随"));
        }
        let mut h = json!({ "Accept": "text/html,application/xhtml+xml", "User-Agent": super::growth::WEB_UA });
        if let Some((cookie, _)) = &cookie {
            h["Cookie"] = json!(cookie);
        }
        let path = format!(
            "{}{}",
            url.path(),
            url.query().map(|q| format!("?{q}")).unwrap_or_default()
        );
        let response = network::open("GET", endpoints::ORIGIN, &path, h, None)?;
        for header in response.header_values("set-cookie") {
            if let Some(value) = parse_cookie(header) {
                cookie = Some(value);
            }
        }
        let redirect = (300..400).contains(&response.status);
        if !(200..400).contains(&response.status) && cookie.is_none() {
            return Err(Error::http(response.status.max(400), "官方网页登录被拒绝"));
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
        cookie.ok_or_else(|| Error::http(502, "官网未返回有效session Cookie"))?;
    let saved = json!({ "cookie": cookie, "expiresAt": expires, "authority": account::authority(&latest.secret), "origin": endpoints::ORIGIN });
    for _ in 0..3 {
        let now = session::snapshot()?;
        if !account::same_auth(&now.secret, &latest.secret) {
            return Err(Error::http(409, "官网登录期间凭据已变化，未保存旧Cookie"));
        }
        let mut secret = now.record["secret"].clone();
        secret[KEY] = saved.clone();
        if account::save(&now.record, &secret)? {
            return Ok(saved);
        }
    }
    Err(Error::http(409, "官网Cookie未能保存，未继续使用未落库状态"))
}
pub fn request(
    current: &Session,
    method: &str,
    path: &str,
    body: Option<&Value>,
    extra: Value,
) -> Result<network::Reply> {
    let saved = ensure(current, None)?;
    session::check(current)?;
    let cookie = saved["cookie"]
        .as_str()
        .ok_or_else(|| Error::http(502, "官网Cookie缺失"))?;
    let mut h = json!({ "Cookie": cookie, "Accept": "application/json, text/plain, */*", "User-Agent": super::growth::WEB_UA,
        "Origin": endpoints::ORIGIN, "Referer": format!("{}/profile/growth-center", endpoints::ORIGIN) });
    if let Some(extra) = extra.as_object() {
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
                && !value
                    .as_str()
                    .and_then(|s| url::Url::parse(s).ok())
                    .is_some_and(|u| u.origin().ascii_serialization() == endpoints::ORIGIN)
            {
                continue;
            }
            h[key] = value.clone();
        }
    }
    // 每次业务调用只提交一次；401、失败或响应未确认都不刷新Cookie后重发。
    let mut reply = network::raw(method, endpoints::ORIGIN, path, h, body)?;
    if crate::errors::failed(reply.status, &reply.value) {
        let code = crate::errors::code(&reply.value);
        let invalid_cookie = reply.status == 401 || code == Some(12153);
        let message = crate::errors::clean(
            crate::errors::message(&reply.value),
            &[cookie, current.token(), current.refresh()],
        );
        reply.value = json!({ "code": code, "message": message, "error": { "message": message } });
        if invalid_cookie {
            // 只丢弃确实被拒的同一Cookie；不在本次业务调用重发或覆盖另一轮新会话。
            for _ in 0..3 {
                let latest = session::snapshot()?;
                if account::authority(&latest.secret) != account::authority(&current.secret)
                    || latest.record["secret"][KEY] != saved
                {
                    break;
                }
                let mut secret = latest.record["secret"].clone();
                if let Some(fields) = secret.as_object_mut() {
                    fields.remove(KEY);
                }
                if account::save(&latest.record, &secret)? {
                    break;
                }
            }
        }
    }
    Ok(reply)
}
