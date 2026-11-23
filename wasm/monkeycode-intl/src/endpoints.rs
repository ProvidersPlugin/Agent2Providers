//! 2026-11-10：国际独立基础包。来源与修改说明见 licenses/SOURCES.json。
//! 只允许精确国际来源和五类只读 GET；不接受调用方 URL 或地区切换。

use agent2api_wasm_sdk::{Error, Result};
use std::collections::BTreeSet;

pub const ID: &str = "monkeycode-intl";
pub const ORIGIN: &str = "https://monkeycode-ai.net";
pub const DOMAIN: &str = "monkeycode-ai.net";
pub const SESSION_COOKIE: &str = "monkeycode_ai_session";

#[derive(Clone, Copy)]
pub enum ReadEndpoint {
    Status,
    Models,
    Wallet,
    Transactions,
    Subscription,
}

impl ReadEndpoint {
    pub fn path(self) -> &'static str {
        match self {
            Self::Status => "/api/v1/users/status",
            Self::Models => "/api/v1/users/models",
            Self::Wallet => "/api/v1/users/wallet",
            Self::Transactions => "/api/v1/users/wallet/transaction",
            Self::Subscription => "/api/v1/users/subscription",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Status => "身份校验",
            Self::Models => "模型目录",
            Self::Wallet => "钱包",
            Self::Transactions => "流水",
            Self::Subscription => "订阅",
        }
    }

    pub fn url(self, query: &[(&str, String)]) -> Result<String> {
        let allowed: &[&str] = match self {
            Self::Models => &["cursor", "limit"],
            Self::Transactions => &["page", "size", "next_token"],
            _ => &[],
        };
        let mut seen = BTreeSet::new();
        if query.len() > allowed.len()
            || query.iter().any(|(key, value)| {
                !allowed.contains(key)
                    || !seen.insert(*key)
                    || value.is_empty()
                    || value.len() > 4096
                    || value.chars().any(char::is_control)
            })
        {
            return Err(Error::http(400, "MonkeyCode 国际只读查询参数无效"));
        }
        let mut url = format!("{ORIGIN}{}", self.path());
        if !query.is_empty() {
            let mut encoded = url::form_urlencoded::Serializer::new(String::new());
            for (key, value) in query {
                encoded.append_pair(key, value);
            }
            url.push('?');
            url.push_str(&encoded.finish());
        }
        Ok(url)
    }
}
