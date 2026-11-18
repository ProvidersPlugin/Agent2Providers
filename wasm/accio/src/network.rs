//! 业务 GET 的 accessToken 在 query，POST 凭证在 body；绝不添加 Authorization。
//! 调试报文的 query/body/sgcookie 脱敏属于宿主边界，已报告，不改写上游鉴权规避。

use crate::{credentials::Credentials, endpoints, errors};
use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};

pub fn header<'a>(headers: &'a Value, name: &str) -> &'a str {
    headers
        .as_object()
        .into_iter()
        .flatten()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .and_then(|(_, value)| value.as_str())
        .unwrap_or("")
}

fn os() -> Result<&'static str> {
    match sdk::host("runtime.platform", json!({}))?.as_str() {
        Some("win") => Ok("windows"),
        Some("mac") => Ok("macos"),
        Some("linux") => Ok("linux"),
        _ => Err(Error::http(503, "主程序没有提供 Accio 所需的系统类型")),
    }
}

pub fn headers() -> Result<Value> {
    Ok(
        json!({ "Content-Type": "application/json", "Accept": "application/json", "Accept-Language": "en",
        "x-language": "en", "x-platform": "desktop", "x-os": os()?, "x-app-version": endpoints::VERSION,
        "x-package-region": endpoints::REGION, "User-Agent": endpoints::USER_AGENT }),
    )
}

pub fn chat_headers(credential: &Credentials) -> Result<Value> {
    let mut headers = headers()?;
    headers["Accept"] = json!("text/event-stream");
    headers["appKey"] = json!(endpoints::APP_KEY);
    headers["version"] = json!(endpoints::VERSION);
    headers["utdid"] = json!(credential.device);
    if !credential.sgcookie.is_empty() {
        headers["sgcookie"] = json!(credential.sgcookie);
    }
    Ok(headers)
}

pub fn read(response: http::Response, secrets: &[&str], limit: usize) -> Result<Value> {
    let status = response.status;
    let mime = header(&response.headers, "content-type").to_ascii_lowercase();
    if header(&response.headers, "bxpunish") == "1"
        || mime.contains("text/html")
        || mime.contains("application/xhtml+xml")
    {
        return Err(Error::http(
            403,
            "Accio 返回网页验证或风控拦截，请在官方客户端确认账号状态",
        )
        .with_code("accio_challenge_required"));
    }
    let bytes = response.bytes(limit).map_err(errors::transport)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        // 不回显未解析的响应正文，以免它含 query、凭证或一次性授权码。
        errors::upstream(status, "", "上游未返回有效 JSON", secrets)
    })?;
    let code = errors::code(&value);
    let message = errors::message(&value);
    let failed = value["success"] == false
        || !code.is_empty() && !matches!(code.as_str(), "0" | "200")
        || message.eq_ignore_ascii_case("NOT_LOGIN")
        || value.get("error").is_some_and(|error| {
            !matches!(error, Value::Null | Value::Bool(false))
                && error.as_str().is_none_or(|text| !text.is_empty())
        });
    if !(200..300).contains(&status) || failed {
        return Err(errors::upstream(status, &code, &message, secrets));
    }
    if !value.is_object() {
        return Err(Error::http(502, "Accio 上游没有返回有效对象"));
    }
    Ok(value)
}

pub fn data(value: Value) -> Value {
    value
        .get("data")
        .filter(|data| data.is_object())
        .cloned()
        .unwrap_or(value)
}

pub fn get(credential: &Credentials, path: &str) -> Result<Value> {
    let url = format!(
        "{}{}?accessToken={}",
        endpoints::GATEWAY,
        path,
        crate::util::query(&credential.access)
    );
    read(
        http::open("GET", &url, headers()?, None).map_err(errors::transport)?,
        &credential.secrets(),
        2 * 1024 * 1024,
    )
}

pub fn post(path: &str, body: &Value, secrets: &[&str]) -> Result<Value> {
    read(
        http::open(
            "POST",
            &format!("{}{path}", endpoints::GATEWAY),
            headers()?,
            Some(body),
        )
        .map_err(errors::transport)?,
        secrets,
        4 * 1024 * 1024,
    )
}

pub fn profile(credential: &Credentials) -> Result<Value> {
    if endpoints::ID == "accio-cn" {
        // 当前中国站客户端的 userinfo 只读接口使用 POST，参数名为 accessToken。
        // 仅旧部署明确拒绝该方法时尝试原 GET，其他错误不重复或改写鉴权方式。
        match post(
            endpoints::PROFILE,
            &json!({ "accessToken": credential.access }),
            &credential.secrets(),
        ) {
            Ok(value) => return Ok(data(value)),
            Err(error) if error.status == Some(405) => {}
            Err(error) => return Err(error),
        }
    }
    get(credential, endpoints::PROFILE).map(data)
}
