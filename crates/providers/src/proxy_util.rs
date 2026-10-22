//! 账号级代理配置的解析（网关 `core::proxies::resolve_account_proxy` 的插件侧等价物）。
//!
//! ── 为什么需要这个 ──────────────────────────────────────────
//! 账号记录里的 `proxy` 字段是**用户填的配置**（`{source, url}` 等形态），而
//! 协议里的 `WireProxy` 是**已解析的出站代理**（protocol / host / port / 认证）。
//! 搬来的家（Qoder / CodeArts）的 `account_proxy(record)` 读的是前者，因此
//! 插件里要有一份「配置 → 出站」的翻译。
//!
//! ── 插件里只支持直连与静态地址（如实说明）─────────────────────
//! 网关那份还认「跟随系统代理」「按请求轮询多条」「从代理池挑一条」这几种
//! `source`。那几种要么需要网关的代理池状态（`proxy_rotate` 的游标），要么
//! 需要读系统代理设置 —— 两者都是**网关的状态**，插件进程里没有。因此：
//!   - `source` 是 `direct` / 缺省 / 空 → `None`（直连）；
//!   - 给了可解析的 URL → `Resolved`；
//!   - 其余（`system` / `rotate` / `pool`）→ `Failed`，文案说清「插件不支持，
//!     请在账号上直接填代理 URL」。
//!
//! 这是**已知的能力差**，不是疏漏：要让插件支持轮询，得让网关把「这次该走
//! 哪条」解析好放进请求（转发链路上它本来就在 `ConversationRequest.proxy`
//! 里 —— 插件只是在「目录刷新 / 余额」这类旁路上自己读配置）。

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
    let Some(object) = config.as_object() else {
        return Some(ProxyResolution::Failed(
            "不支持的代理来源: undefined".to_string(),
        ));
    };
    let source = object
        .get("source")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_lowercase();
    // 直连档：没有 source 或显式 direct
    if source.is_empty() || source == "direct" {
        return None;
    }
    if matches!(source.as_str(), "system" | "rotate" | "pool") {
        return Some(ProxyResolution::Failed(format!(
            "插件不支持「{source}」代理来源（它依赖网关的代理池状态）：请在账号上直接填写代理地址"
        )));
    }
    // 静态档：从配置里取 URL
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
