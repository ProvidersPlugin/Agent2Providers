//! 出网代理的线上形态。
//!
//! ── 为什么需要它（而不是让插件自己读配置）────────────────────
//! 代理是**账号级**的（同一个 provider 的两个账号可以走不同出口），也可能
//! 来自网关的全局设置。插件不认识账号存储里的 `proxy` 字段形状，也不该
//! 认识 —— 那是网关的落盘契约。所以网关解析好之后，用这个结构过线。
//!
//! ── 三态（与网关 `ProxyResolution` 对齐）────────────────────
//!   - `None`：跟随网关默认出口（`HostEnv.proxy`）；
//!   - `Some(proxy)`：显式出口，按字段建客户端；
//!   - 配了但数据坏了的情形**不会到这里** —— 网关在解析阶段就降级成
//!     「跟随默认」并记一条 notice（可用性优先，与 `ResolvedProxy::from_json`
//!     的第三态说明一致）。

use serde::{Deserialize, Serialize};

/// 一个已解析的出网代理。
///
/// 字段与网关的 `core::proxies::ResolvedProxy` 一一对应，但**没有** `source`
/// 与 `label`：那两个是给网关界面显示「这个出口是谁配的」用的，插件不需要，
/// 传过去只会诱使插件去解析它们。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireProxy {
    /// 协议：`http` / `https` / `socks5`。
    pub protocol: String,
    /// 主机（域名或 IP）。
    pub host: String,
    /// 端口。`None` = 该协议的默认端口（由插件侧决定）。
    pub port: Option<u16>,
    /// 用户名（无认证时为空串）。
    #[serde(default)]
    pub username: String,
    /// 密码（无认证时为空串）。
    #[serde(default)]
    pub password: String,
}

impl WireProxy {
    /// 拼成标准 URL 形态（`scheme://[user:pass@]host:port`）。
    ///
    /// ── 为什么要 URL 编码用户名与密码 ──────────────────────────
    /// 用户填的密码里出现 `@`、`:`、`/` 是常事，直接拼会让 URL 解析器把
    /// 后半截当成主机名 —— 症状是「代理连不上」而配置看起来完全正常。
    /// 这里做最小百分号编码（只编那些在 userinfo 里有特殊含义的字符），
    /// 不做完整的 `url::Url` 依赖（协议 crate 不带这个依赖）。
    pub fn to_url(&self) -> String {
        let scheme = if self.protocol.is_empty() { "http" } else { &self.protocol };
        let port = match self.port {
            Some(p) => format!(":{p}"),
            None => String::new(),
        };
        if self.username.is_empty() && self.password.is_empty() {
            format!("{scheme}://{}{port}", self.host)
        } else {
            format!(
                "{scheme}://{}:{}@{}{port}",
                encode_userinfo(&self.username),
                encode_userinfo(&self.password),
                self.host
            )
        }
    }

    /// 是否是 SOCKS5（网关侧要区别对待：reqwest 支持，但有些客户端不支持）。
    pub fn is_socks5(&self) -> bool {
        matches!(self.protocol.as_str(), "socks5" | "socks5h")
    }
}

/// userinfo 段的最小百分号编码。
///
/// 只编码在 URL 的 userinfo 里有特殊含义的字符，以及控制字符与非 ASCII。
/// 其余原样保留 —— 过度编码会让「密码里有个 `%`」变成「密码变了」，
/// 而用户复制粘贴的密码通常已经是他看到的那个形态。
fn encode_userinfo(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'@' | b':' | b'/' | b'?' | b'#' | b'[' | b']' | b' ' | b'%' => {
                out.push('%');
                out.push_str(&format!("{byte:02X}"));
            }
            0x00..=0x1f | 0x7f => {
                out.push('%');
                out.push_str(&format!("{byte:02X}"));
            }
            0x80..=0xff => {
                out.push('%');
                out.push_str(&format!("{byte:02X}"));
            }
            _ => out.push(*byte as char),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_proxy_url() {
        let p = WireProxy {
            protocol: "http".into(),
            host: "127.0.0.1".into(),
            port: Some(7890),
            username: String::new(),
            password: String::new(),
        };
        assert_eq!(p.to_url(), "http://127.0.0.1:7890");
    }

    #[test]
    fn proxy_with_credentials_encodes_specials() {
        let p = WireProxy {
            protocol: "socks5".into(),
            host: "proxy.example.com".into(),
            port: Some(1080),
            username: "user@corp".into(),
            password: "p@ss:w/rd".into(),
        };
        assert_eq!(
            p.to_url(),
            "socks5://user%40corp:p%40ss%3Aw%2Frd@proxy.example.com:1080"
        );
        assert!(p.is_socks5());
    }

    #[test]
    fn missing_port_omits_it() {
        let p = WireProxy {
            protocol: String::new(),
            host: "h".into(),
            port: None,
            username: String::new(),
            password: String::new(),
        };
        assert_eq!(p.to_url(), "http://h");
    }
}
