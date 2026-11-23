//! 静态请求用 SDK HTTP，五条动态模型/余额/奖励路径只走 http::open_account。
//! 调用方不能指定动态URL；签名地址按同一账号基址生成，实际目的地由宿主独立解析。
//! 账号来源及代理由宿主核对，不会降级普通HTTP或偷偷改直连。

use super::{
    credentials::Credentials,
    endpoints, errors,
    signer::{self, Algorithm},
};
use agent2api_wasm_sdk::{http, json, Error, Result, Value};
use base64::Engine;

pub struct Reply {
    pub status: u16,
    pub value: Value,
}

pub fn open_raw(
    method: &str,
    url: &str,
    headers: Value,
    body: Option<&[u8]>,
) -> Result<http::Response> {
    if endpoints::is_gateway(url) {
        return Err(Error::http(403, "模型网关必须使用当前账号的受控来源授权"));
    }
    http::open_raw(method, url, headers, body).map_err(|mut error| {
        error.message = "OfficeAce 静态接口网络请求未完成".into();
        error
    })
}

fn open_account(
    method: &str,
    path: &str,
    headers: Value,
    body: Option<&Value>,
) -> Result<http::Response> {
    http::open_account(method, path, headers, body).map_err(|mut error| {
        // 不把宿主地址/权限错误包装成认证过期，不触发STS续期重试。
        match error.code.as_deref() {
            Some("account_origin_authorization_required") => {
                error.status = Some(403);
                error.message = "OfficeAce 账号服务器来源不可用，请检查登录凭据并重新导入账号".into();
            }
            Some(code) if code.contains("forbidden") || code.contains("origin") || code.contains("proxy") => {
                error.message = "OfficeAce 账号服务器访问被拦截，请检查账号来源和网络设置".into();
            }
            _ => { error.message = "OfficeAce 账号网关请求未完成".into(); }
        }
        error
    })
}

pub fn read(response: http::Response, allow_empty: bool) -> Result<Reply> {
    let status = response.status;
    let bytes = response.bytes(4 * 1024 * 1024).map_err(|mut error| {
        error.message = "OfficeAce 响应读取失败".into();
        error
    })?;
    if (allow_empty || status == 202) && bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Reply {
            status,
            value: Value::Null,
        });
    }
    let value = serde_json::from_slice(&bytes).map_err(|_| {
        Error::http(
            if (400..600).contains(&status) {
                status
            } else {
                502
            },
            "OfficeAce 接口响应不是有效 JSON",
        )
    })?;
    Ok(Reply { status, value })
}

pub fn plain(method: &str, url: &str, body: Option<&Value>) -> Result<Reply> {
    let body = body.map(Value::to_string);
    read(
        open_raw(
            method,
            url,
            json!({ "accept": "application/json", "content-type": "application/json" }),
            body.as_deref().map(str::as_bytes),
        )?,
        false,
    )
}

pub fn basic(credential: &Credentials) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!(
            "{}:{}",
            credential.model_app_key, credential.model_app_secret
        ))
    )
}

pub fn model(
    method: &str,
    path: &str,
    credential: &Credentials,
    body: Option<&Value>,
    extra: Value,
) -> Result<http::Response> {
    credential.forwarding()?;
    if !matches!(
        (method, path),
        ("GET", "/v2/models") | ("POST", "/v2/chat/completions")
    ) {
        return Err(Error::http(400, "OfficeAce Basic 凭据不能用于该接口"));
    }
    let mut headers = extra;
    headers["authorization"] = json!(basic(credential));
    if body.is_some() {
        headers["content-type"] = json!("application/json");
    }
    open_account(method, path, headers, body)
}

fn signed_headers(
    credential: &Credentials,
    method: &str,
    url: &str,
    algorithm: Algorithm,
    body: Option<&Value>,
    mut extra: Value,
) -> Result<Value> {
    credential.control()?;
    let body = body.map(Value::to_string);
    if body.is_some() {
        extra["content-type"] = json!("application/json;charset=utf8");
    }
    signer::sign(
        &credential.signing,
        algorithm,
        method,
        url,
        body.as_deref().unwrap_or("").as_bytes(),
        &extra,
    )
}

fn signed_result(
    response: Result<http::Response>,
    credential: &Credentials,
    method: &str,
    empty: bool,
) -> Result<Value> {
    let response = response.map_err(|error| errors::uncertain(method, error))?;
    let reply = read(response, empty).map_err(|error| errors::uncertain(method, error))?;
    if let Some(error) = errors::control_response(reply.status, &reply.value, credential) {
        return Err(if reply.status >= 500 || reply.status == 408 {
            errors::uncertain(method, error)
        } else {
            error
        });
    }
    Ok(reply.value)
}

pub fn signed(
    credential: &Credentials,
    method: &str,
    url: &str,
    algorithm: Algorithm,
    body: Option<&Value>,
    extra: Value,
) -> Result<Value> {
    let headers = signed_headers(credential, method, url, algorithm, body, extra)?;
    let bytes = body.map(Value::to_string);
    signed_result(
        open_raw(method, url, headers, bytes.as_deref().map(str::as_bytes)),
        credential,
        method,
        method == "POST" && url.ends_with("/v1/agreement"),
    )
}

pub fn gateway_control(
    credential: &Credentials,
    method: &str,
    path: &str,
    body: Option<&Value>,
    version: bool,
) -> Result<Value> {
    let url = endpoints::gateway(&credential.base_url, path)?;
    let extra = if version {
        json!({ "x-subscription-type": "v2" })
    } else {
        json!({})
    };
    let headers = signed_headers(credential, method, &url, Algorithm::V11, body, extra)?;
    signed_result(
        open_account(method, path, headers, body),
        credential,
        method,
        false,
    )
}

pub fn cloud(
    credential: &Credentials,
    method: &str,
    path: &str,
    body: Option<&Value>,
    version: bool,
) -> Result<Value> {
    let extra = if version {
        json!({ "x-subscription-type": "v2" })
    } else {
        json!({})
    };
    signed(
        credential,
        method,
        &format!("{}{path}", endpoints::CLOUD),
        Algorithm::Sdk,
        body,
        extra,
    )
}
