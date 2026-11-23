//! 每次账号操作独立绑定出口；同一操作里的续期、查询与领取共享它。

use std::future::Future;

use agent2api_provider_protocol as proto;
use serde_json::Value;

use crate::{PluginCtx, StoreAccount, StoreError, StoreProxy};

#[derive(Clone)]
pub enum Route {
    Direct,
    Proxy(proto::WireProxy),
}

impl Route {
    pub fn url(&self) -> String {
        match self {
            Self::Direct => "direct".to_string(),
            Self::Proxy(proxy) => proxy_url(proxy),
        }
    }
}

#[derive(Clone)]
pub struct AccountRoute {
    provider: String,
    account: String,
    route: Option<Route>,
    user_agent: String,
}

tokio::task_local! {
    static CURRENT: Option<AccountRoute>;
}

pub fn current() -> Option<Route> {
    CURRENT.try_with(|current| current.as_ref().and_then(|value| value.route.clone()))
        .ok().flatten()
}

pub fn capture() -> Option<AccountRoute> {
    CURRENT.try_with(Clone::clone).ok().flatten()
}

pub fn user_agent() -> String {
    CURRENT.try_with(|current| current.as_ref().map(|value| value.user_agent.clone()).unwrap_or_default())
        .unwrap_or_default()
}

pub async fn scope<F: Future>(route: Option<AccountRoute>, work: F) -> F::Output {
    CURRENT.scope(route, work).await
}

pub fn proxy_url(proxy: &proto::WireProxy) -> String {
    let mut proxy = proxy.clone();
    if proxy.protocol == "socks5" { proxy.protocol = "socks5h".to_string(); }
    if proxy.host.contains(':') && !proxy.host.starts_with('[') {
        proxy.host = format!("[{}]", proxy.host);
    }
    proxy.to_url()
}

pub fn from_account(account: &StoreAccount) -> Result<AccountRoute, String> {
    let route = match &account.proxy {
        Some(StoreProxy::Unset) => None,
        Some(StoreProxy::Resolved { proxy }) => Some(Route::Proxy(validate(proxy.clone())?)),
        Some(StoreProxy::Static { config }) => parse_config(config)?,
        Some(StoreProxy::Failed { .. }) => return Err(unavailable()),
        None => {
            let secret = match &account.secret {
                Value::String(text) => serde_json::from_str(text).unwrap_or(Value::Null),
                value => value.clone(),
            };
            parse_config(secret.get("proxy").unwrap_or(&Value::Null))?
        }
    };
    Ok(AccountRoute {
        provider: account.provider.clone(),
        account: account.id.clone(),
        route,
        user_agent: account.public.get("userAgent").and_then(Value::as_str).unwrap_or("").trim().to_string(),
    })
}

pub async fn for_account(
    ctx: &PluginCtx,
    provider: &str,
    account: &str,
) -> Result<AccountRoute, String> {
    // 新账号边界不能继承外层账号的出口，批量任务也逐账号重新解析。
    let stored = scope(None, ctx.store_get(provider, account)).await
        .map_err(|error| error.to_string())?;
    from_account(&stored)
}

pub(crate) async fn for_request(
    ctx: &PluginCtx,
    method: &str,
    params: &Value,
) -> Result<Option<AccountRoute>, String> {
    use proto::methods::gateway as methods;
    let provider = params.get("provider").and_then(Value::as_str).unwrap_or("");
    let account = params.get("account").and_then(Value::as_str).unwrap_or("");
    let network = matches!(method,
        methods::CHAT | methods::CONVERSATION | methods::TOKEN_ENSURE
        | methods::TOKEN_REFRESH | methods::MODELS_REFRESH
        | methods::USAGE_QUERY | methods::CHECKIN_CLAIM | methods::CLAIM
        | methods::CLAIM_STATUS | crate::images::GENERATE_METHOD | crate::images::EDIT_METHOD
    ) || (method == methods::ADMIN_ACTION && !account.is_empty());
    if !network {
        return Ok(None);
    }
    let explicit = if params.get("proxy_override").and_then(Value::as_bool) == Some(true) {
        Some(Route::Direct)
    } else {
        parse_config(params.get("proxy").unwrap_or(&Value::Null))?
    };
    match ctx.store_get(provider, account).await {
        Ok(stored) => {
            let resolved = if let Some(route) = explicit {
                AccountRoute {
                    provider: stored.provider.clone(), account: stored.id.clone(), route: Some(route),
                    user_agent: stored.public.get("userAgent").and_then(Value::as_str).unwrap_or("").trim().to_string(),
                }
            } else { from_account(&stored)? };
            Ok(Some(resolved))
        }
        Err(StoreError::NotFound) if account.is_empty() => Ok(explicit.map(|route| AccountRoute {
            provider: provider.to_string(), account: String::new(), route: Some(route), user_agent: String::new(),
        })),
        Err(error) => Err(error.to_string()),
    }
}

pub(crate) fn apply(account: &mut StoreAccount) {
    let Some(current) = capture() else { return };
    if current.provider != account.provider || current.account != account.id {
        return;
    }
    account.proxy = Some(match current.route {
        Some(Route::Direct) => StoreProxy::Static { config: serde_json::json!({ "source": "direct" }) },
        Some(Route::Proxy(proxy)) => StoreProxy::Resolved { proxy },
        None => StoreProxy::Unset,
    });
}

fn parse_config(config: &Value) -> Result<Option<Route>, String> {
    if config.is_null() || config.as_str() == Some("") {
        return Ok(None);
    }
    if let Some(inner) = config.get("config") {
        return parse_config(inner);
    }
    let Some(fields) = config.as_object() else { return Err(unavailable()) };
    if fields.is_empty() { return Ok(None); }
    let source = config.get("source").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
    if source == "direct" { return Ok(Some(Route::Direct)); }
    if config.get("error").is_some() || matches!(source.as_str(), "pool" | "rotate" | "clash" | "system") {
        return Err(unavailable());
    }
    if let Some(host) = config.get("host").and_then(Value::as_str).filter(|value| !value.trim().is_empty()) {
        let port = match config.get("port") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_u64()
                .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
                .and_then(|number| u16::try_from(number).ok())
                .filter(|port| *port > 0).ok_or_else(unavailable)?),
        };
        let proxy = proto::WireProxy {
            protocol: config.get("protocol").and_then(Value::as_str).unwrap_or("http").trim().to_lowercase(),
            host: host.trim().trim_matches(|ch| ch == '[' || ch == ']').to_string(),
            port,
            username: config.get("username").and_then(Value::as_str).unwrap_or("").to_string(),
            password: config.get("password").and_then(Value::as_str).unwrap_or("").to_string(),
        };
        return validate(proxy).map(|proxy| Some(Route::Proxy(proxy)));
    }
    for key in ["url", "proxy", "address", "server"] {
        let Some(text) = config.get(key).and_then(Value::as_str).filter(|text| !text.trim().is_empty()) else { continue };
        let url = url::Url::parse(text.trim()).map_err(|_| unavailable())?;
        if url.path() != "/" && !url.path().is_empty() || url.query().is_some() || url.fragment().is_some() {
            return Err(unavailable());
        }
        let decode = |text: &str| percent_encoding::percent_decode_str(text).decode_utf8()
            .map(|text| text.into_owned()).map_err(|_| unavailable());
        let proxy = proto::WireProxy {
            protocol: url.scheme().to_string(),
            host: url.host_str().ok_or_else(unavailable)?.trim_matches(|ch| ch == '[' || ch == ']').to_string(),
            port: url.port(),
            username: decode(url.username())?,
            password: decode(url.password().unwrap_or(""))?,
        };
        return validate(proxy).map(|proxy| Some(Route::Proxy(proxy)));
    }
    Err(unavailable())
}

fn validate(mut proxy: proto::WireProxy) -> Result<proto::WireProxy, String> {
    if proxy.protocol.is_empty() { proxy.protocol = "http".to_string(); }
    if !matches!(proxy.protocol.as_str(), "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h")
        || proxy.host.trim().is_empty() || proxy.port == Some(0)
    {
        return Err(unavailable());
    }
    let url = url::Url::parse(&proxy_url(&proxy)).map_err(|_| unavailable())?;
    if url.host_str().is_none() || !matches!(url.path(), "" | "/")
        || url.query().is_some() || url.fragment().is_some()
    {
        return Err(unavailable());
    }
    Ok(proxy)
}

fn unavailable() -> String {
    crate::control_error(
        502,
        "账号代理不可用，请检查账号的代理设置；本次操作未改走直连",
        Some("account_proxy_unavailable".to_string()),
        None,
    )
}
