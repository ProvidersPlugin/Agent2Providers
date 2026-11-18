//! 国内唯一来源；本批的出站枚举只有只读 GET，不包含任务、验证码或消费接口。

use agent2api_wasm_sdk::{Error, Result};

pub const ID: &str = "monkeycode";
pub const ORIGIN: &str = "https://monkeycode-ai.com";
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
            Self::Status => "登录状态校验",
            Self::Models => "模型目录查询",
            Self::Wallet => "钱包查询",
            Self::Transactions => "流水查询",
            Self::Subscription => "订阅查询",
        }
    }

    pub fn url(self, query: &[(&str, String)]) -> Result<String> {
        let allowed: &[&str] = match self {
            Self::Models => &["cursor", "limit"],
            Self::Transactions => &["page", "size", "next_token"],
            _ => &[],
        };
        if query.len() > allowed.len()
            || query.iter().any(|(key, value)| {
                !allowed.contains(key) || value.len() > 4096 || value.chars().any(char::is_control)
            })
        {
            return Err(Error::http(400, "MonkeyCode 只读查询参数无效"));
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
