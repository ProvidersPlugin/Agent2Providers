use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{PluginCtx, StoreProxy};
use reqwest::header;
use serde_json::{json, Value};

use super::PROVIDER_ID;
use crate::errors::GatewayError;

pub const BASE_URL: &str = "https://www.doubao.com";
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36";
// 上游按 pc_version 下发模型菜单；只有 runtime_version 时会成功返回空菜单。
const DEFAULT_CLIENT_VERSION: &str = "2.33.5";

pub fn client(
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
    stream: bool,
) -> Result<reqwest::Client, GatewayError> {
    type Key = (String, Option<crate::timeouts::Timeouts>);
    static CLIENTS: OnceLock<Mutex<HashMap<Key, reqwest::Client>>> = OnceLock::new();
    let proxy = proxy
        .map(proto::WireProxy::to_url)
        .unwrap_or_else(|| gateway_proxy.trim().to_string());
    let timeouts = stream.then(crate::timeouts::current);
    let key = (proxy.clone(), timeouts);
    let clients = CLIENTS.get_or_init(Mutex::default);
    if let Some(client) = clients
        .lock()
        .ok()
        .and_then(|clients| clients.get(&key).cloned())
    {
        return Ok(client);
    }
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(15));
    builder = match timeouts {
        Some(timeouts) => builder
            .connect_timeout(Duration::from_millis(timeouts.connect_ms()))
            .read_timeout(Duration::from_millis(timeouts.read_timeout_backstop_ms())),
        None => builder.timeout(Duration::from_secs(30)),
    };
    if !proxy.is_empty() && proxy != "direct" {
        let proxy = reqwest::Proxy::all(&proxy)
            .map_err(|_| GatewayError::with_status(400, "豆包工作代理地址无效"))?;
        builder = builder.proxy(proxy);
    }
    let client = builder
        .build()
        .map_err(|_| GatewayError::with_status(502, "无法创建豆包工作出网连接"))?;
    if let Ok(mut clients) = clients.lock() {
        if clients.len() >= 128 {
            clients.clear();
        }
        clients.insert(key, client.clone());
    }
    Ok(client)
}

pub fn number(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.trim().parse().ok())
}

pub fn identifier(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_u64().map(|id| id.to_string()))
        .filter(|id| !id.trim().is_empty())
}

pub fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

pub fn headers(credentials: &Value) -> Result<Vec<(String, String)>, GatewayError> {
    let cookie = super::accounts::cookie_header(credentials)?;
    Ok(vec![
        (header::COOKIE.as_str().into(), cookie),
        (header::ORIGIN.as_str().into(), BASE_URL.into()),
        (header::REFERER.as_str().into(), format!("{BASE_URL}/chat/")),
        (
            header::USER_AGENT.as_str().into(),
            credentials["userAgent"]
                .as_str()
                .unwrap_or(USER_AGENT)
                .into(),
        ),
        ("Agw-Js-Conv".into(), "str".into()),
    ])
}

pub fn request(
    client: &reqwest::Client,
    credentials: &Value,
    path: &str,
) -> Result<reqwest::RequestBuilder, GatewayError> {
    let client_version = credentials["clientVersion"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_CLIENT_VERSION);
    let mut query = vec![
        ("aid", "1044603".to_string()),
        ("real_aid", "1044603".into()),
        ("device_platform", "web".into()),
        ("version_code", "20800".into()),
        ("samantha_web", "1".into()),
        ("web_platform", "desktop".into()),
        ("doubao_device_platform", "desktop".into()),
        ("client_platform", "pc_client".into()),
        ("pkg_type", "release_version".into()),
        ("runtime", "web".into()),
        ("runtime_version", "3.42.2".into()),
        ("pc_version", client_version.to_string()),
        ("doubao_pc_version", client_version.to_string()),
        ("use-olympus-account", "1".into()),
        ("language", "zh".into()),
        ("region", "CN".into()),
        ("sys_region", "CN".into()),
    ];
    for (wire, field) in [
        ("device_id", "deviceId"),
        ("web_id", "webId"),
        ("chromium_version", "chromiumVersion"),
        ("channel", "channel"),
    ] {
        if let Some(value) = credentials[field]
            .as_str()
            .filter(|value| !value.is_empty())
        {
            query.push((wire, value.into()));
        }
    }
    if let Some(device) = credentials["deviceId"]
        .as_str()
        .filter(|value| !value.is_empty())
    {
        query.push(("fp", format!("verify_{device}")));
    } else if let Some(fingerprint) = credentials["fingerprint"]
        .as_str()
        .filter(|value| !value.is_empty())
    {
        query.push(("fp", fingerprint.into()));
    }
    let mut builder = client.post(format!("{BASE_URL}{path}")).query(&query);
    for (key, value) in headers(credentials)? {
        builder = builder.header(key, value);
    }
    Ok(builder)
}

pub fn error(status: u16, value: &Value) -> GatewayError {
    let detail = value
        .get("error_detail")
        .filter(|value| value.is_object())
        .or_else(|| value.get("error").filter(|value| value.is_object()))
        .unwrap_or(value);
    let code = number(&detail["code"])
        .or_else(|| number(&detail["error_code"]))
        .or_else(|| number(&value["code"]))
        .or_else(|| number(&value["error_code"]));
    let message = ["message", "msg", "error_msg", "error_message", "detail"]
        .iter()
        .find_map(|key| detail[*key].as_str().filter(|text| !text.trim().is_empty()))
        .or_else(|| value.as_str())
        .unwrap_or("上游返回了无法解析的错误");
    let lower = message.to_ascii_lowercase();
    let status = if matches!(code, Some(710012001))
        || status == 401
        || lower.contains("not logged in")
        || lower.contains("login required")
    {
        401
    } else if status == 429 || matches!(code, Some(710022004)) {
        429
    } else if status >= 400 {
        status
    } else {
        502
    };
    GatewayError::with_status(status as i32, format!("豆包工作：{message}"))
        .with_optional_code(code.and_then(|code| i64::try_from(code).ok()))
}

pub fn error_body(error: &GatewayError) -> Value {
    json!({"message":error.message,"code":error.upstream_code,"error":{"message":error.message,"code":error.upstream_code}})
}

pub async fn json_response(
    builder: reqwest::RequestBuilder,
    ug: bool,
) -> Result<Value, GatewayError> {
    let mut response = builder
        .send()
        .await
        .map_err(|error| GatewayError::with_status(502, crate::http::describe(&error)))?;
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| GatewayError::with_status(502, crate::http::describe(&error)))?
    {
        if bytes.len().saturating_add(chunk.len()) > 8 * 1024 * 1024 {
            return Err(GatewayError::with_status(502, "豆包工作控制面响应过大"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        GatewayError::with_status(502, "豆包工作未返回有效 JSON，请检查登录态或稍后重试")
    })?;
    if !(200..300).contains(&status)
        || number(&value["code"]).is_some_and(|code| code != 0 && !(ug && code == 200))
    {
        return Err(error(status, &value));
    }
    Ok(value)
}

pub async fn post(
    client: &reqwest::Client,
    credentials: &Value,
    path: &str,
    body: &Value,
) -> Result<Value, GatewayError> {
    json_response(
        request(client, credentials, path)?.json(body),
        path.starts_with("/alice/ug/"),
    )
    .await
}

pub async fn account(
    ctx: &PluginCtx,
    id: &str,
    stream: bool,
) -> Result<(String, Value, reqwest::Client), GatewayError> {
    let found = ctx.store_get(PROVIDER_ID, id).await.map_err(|error| {
        GatewayError::with_status(401, format!("读取豆包工作账号失败：{error}"))
    })?;
    let proxy = match found.proxy {
        Some(StoreProxy::Resolved { proxy }) => Some(proxy),
        Some(StoreProxy::Static { config }) => {
            match crate::proxy_util::resolve_account_proxy(Some(&config)) {
                Some(crate::proxy_util::ProxyResolution::Resolved(proxy)) => Some(proxy),
                Some(crate::proxy_util::ProxyResolution::Failed(message)) => {
                    return Err(GatewayError::with_status(502, message))
                }
                None => None,
            }
        }
        Some(StoreProxy::Failed { message }) => {
            return Err(GatewayError::with_status(502, message))
        }
        _ => None,
    };
    let credentials = super::accounts::resolve(&found.secret).await?;
    let client = client(proxy.as_ref(), &ctx.env.proxy, stream)?;
    if credentials["expectedUserId"].is_string() {
        let profile = super::accounts::identify(&client, &credentials).await?;
        if profile["userId"] != credentials["expectedUserId"]
            || profile["enterpriseType"] != credentials["expectedEnterpriseType"]
        {
            return Err(GatewayError::with_status(
                401,
                "豆包工作客户端已切换账号，请重新导入当前登录态",
            ));
        }
    }
    Ok((found.id, credentials, client))
}

pub fn classify(status: u16, body: &Value) -> proto::ErrorClass {
    let error = error(status, body);
    if error.status_code == 401 {
        return proto::ErrorClass::TokenExpired {
            message: error.message,
        };
    }
    if error.status_code == 429 {
        return proto::ErrorClass::QuotaLimited {
            reset_at: None,
            message: error.message,
            upstream_code: error.upstream_code,
            status: 429,
        };
    }
    proto::ErrorClass::Fatal {
        status: error.status_code as u16,
        message: error.message,
        upstream_code: error.upstream_code,
    }
}
