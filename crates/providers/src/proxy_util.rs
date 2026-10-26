//! 账号代理的插件侧解析：网关提供实际出口，旧网关回落到静态配置。
//!
//! `store::record_of` 把网关解析结果投影到运行时 `proxy`，原始凭证与代理配置
//! 留在快照里供条件更新。代理池 / Clash / 轮询只由网关解析，插件不读池状态。
//! 没有网关解析结果时，兼容 URL 与协议 / 主机 / 端口两种静态形态；依赖网关
//! 状态的来源仍如实报错。

use serde_json::Value;

use agent2api_provider_protocol::WireProxy;

/// 代理配置的解析结果。
pub enum ProxyResolution {
    /// 解析成功
    Resolved(WireProxy),
    /// 配置存在但用不了（文案给用户看）
    Failed(String),
}

/// 解析账号记录里的 `proxy` 配置。
///
/// `None` = 没配代理（直连）；`Some(Resolved)` = 可用；`Some(Failed)` = 配错了。
pub fn resolve_account_proxy(config: Option<&Value>) -> Option<ProxyResolution> {
    let config = config?;
    if config.is_null() {
        return None;
    }
    if let Some(config) = config.get("config").filter(|value| value.is_object()) {
        return resolve_account_proxy(Some(config));
    }
    let Some(object) = config.as_object() else {
        return Some(ProxyResolution::Failed(
            "不支持的代理来源: undefined".to_string(),
        ));
    };
    if let Some(message) = object
        .get("error")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        return Some(ProxyResolution::Failed(message.to_string()));
    }
    let source = object
        .get("source")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_lowercase();
    if source == "direct" {
        return None;
    }
    if matches!(source.as_str(), "system" | "rotate" | "pool" | "clash") {
        return Some(ProxyResolution::Failed(format!(
            "网关未提供「{source}」代理来源的实际出口，请更新网关与提供商插件"
        )));
    }
    if let Some(host) = object
        .get("host")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|host| !host.is_empty())
    {
        let protocol = object
            .get("protocol")
            .and_then(Value::as_str)
            .unwrap_or("http")
            .trim()
            .to_lowercase();
        if !matches!(protocol.as_str(), "http" | "https" | "socks5" | "socks4") {
            return Some(ProxyResolution::Failed(format!(
                "不支持的代理协议: {protocol}"
            )));
        }
        let port = match object.get("port") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let parsed = value
                    .as_u64()
                    .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()));
                let Some(port) = parsed
                    .and_then(|port| u16::try_from(port).ok())
                    .filter(|port| *port > 0)
                else {
                    return Some(ProxyResolution::Failed(
                        "代理端口必须是 1-65535 的整数".to_string(),
                    ));
                };
                Some(port)
            }
        };
        return Some(ProxyResolution::Resolved(WireProxy {
            protocol,
            host: host.trim_matches(|ch| ch == '[' || ch == ']').to_string(),
            port,
            username: object
                .get("username")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            password: object
                .get("password")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        }));
    }
    for key in ["url", "proxy", "address", "server"] {
        let candidate = object
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if candidate.is_empty() {
            continue;
        }
        return match parse_proxy_url(candidate) {
            Some(proxy) => Some(ProxyResolution::Resolved(proxy)),
            None => Some(ProxyResolution::Failed(format!(
                "代理地址无法解析（{candidate}）：需要形如 http://host:port 或 socks5://host:port"
            ))),
        };
    }
    if source.is_empty() {
        return None;
    }
    Some(ProxyResolution::Failed(format!(
        "代理配置缺少地址（source={source}）"
    )))
}

/// 把代理 URL 拆成 `WireProxy`。
///
/// 手写解析而不是引 `url` crate：只需要 `scheme://[user:pass@]host:port` 这一种
/// 形态，而 `url` crate 会把「端口非法」「用户信息里有 @」这些情况各自归一成
/// 不同错误 —— 这里要的只是「能拆就拆，拆不出就说清」。
pub fn parse_proxy_url(text: &str) -> Option<WireProxy> {
    let text = text.trim();
    let (scheme, rest) = text.split_once("://")?;
    let protocol = scheme.trim().to_lowercase();
    if !matches!(protocol.as_str(), "http" | "https" | "socks5" | "socks4") {
        return None;
    }
    // 用户信息：最后一个 @ 之前的部分（密码里可能有 @）
    let (userinfo, hostport) = match rest.rsplit_once('@') {
        Some((userinfo, hostport)) => (Some(userinfo), hostport),
        None => (None, rest),
    };
    let hostport = hostport.trim_end_matches('/');
    if hostport.is_empty() {
        return None;
    }
    // IPv6 形态 [::1]:8080 也走这条（冒号在后面）
    let (host, port) = match hostport.rsplit_once(':') {
        Some((host, port)) => {
            let port = port.trim();
            if port.is_empty() {
                (host, None)
            } else {
                (host, Some(port.parse::<u16>().ok()?))
            }
        }
        None => (hostport, None),
    };
    if host.is_empty() {
        return None;
    }
    let (username, password) = match userinfo {
        Some(userinfo) => match userinfo.split_once(':') {
            Some((user, pass)) => (decode(user), decode(pass)),
            None => (decode(userinfo), String::new()),
        },
        None => (String::new(), String::new()),
    };
    Some(WireProxy {
        protocol,
        host: host.trim_matches(|ch| ch == '[' || ch == ']').to_string(),
        port,
        username,
        password,
    })
}

/// 最小百分号解码（代理认证信息里常见 `%40` 这类转义）。
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &text[index + 1..index + 3];
            if let Ok(value) = u8::from_str_radix(hex, 16) {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
