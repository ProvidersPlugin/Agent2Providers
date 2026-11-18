//! 公开字符串只作文本白名单投影；敏感字段及 Cookie 内的其它值也不可借姓名回显。
//! 不保存整份上游对象，不返回错误正文。摘要和凭据仅留在私有账号状态。

use agent2api_wasm_sdk::{Error, Result, Value};

#[derive(Clone, Default)]
pub struct Guard {
    secrets: Vec<String>,
}

impl Guard {
    pub fn add(&mut self, value: &str) -> Result<()> {
        if value.is_empty() || self.secrets.iter().any(|secret| secret == value) {
            return Ok(());
        }
        if value.len() > 64 * 1024 || self.secrets.len() >= 1024 {
            return Err(
                Error::http(502, "MonkeyCode 国际受保护字段超过安全投影上限")
                    .with_code("monkeycode_intl_redaction_limit"),
            );
        }
        self.secrets.push(value.to_string());
        self.secrets
            .sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        Ok(())
    }

    pub fn cookie(&mut self, raw: &str) -> Result<()> {
        self.add(raw)?;
        for part in raw.split(';') {
            if let Some((name, value)) = part.split_once('=') {
                let value = value.trim().trim_matches(['\'', '"']);
                let sensitive_name = name.to_ascii_lowercase();
                // 0/1 偏好值不是凭据；普通/团队会话或 token 则无论多短都保护。
                if value.len() >= 8
                    || ["session", "token", "secret", "authorization"]
                        .iter()
                        .any(|key| sensitive_name.contains(key))
                {
                    self.add(value)?;
                }
            }
        }
        Ok(())
    }

    pub fn observe(&mut self, value: &Value) -> Result<()> {
        self.walk(value, false, 0)
    }

    fn walk(&mut self, value: &Value, secret: bool, depth: usize) -> Result<()> {
        if depth > 24 {
            return Err(Error::http(502, "MonkeyCode 国际对象嵌套过深，未下发原文"));
        }
        match value {
            Value::Object(fields) => {
                for (key, value) in fields {
                    let key = key.to_ascii_lowercase();
                    // secret/pluginSecret 是结构容器；只保护其中的凭据键，不能把 userId 当令牌。
                    if ["next_token", "tokentail", "token_tail"].contains(&key.as_str()) {
                        continue;
                    }
                    let container = (["secret", "pluginsecret", "credentials", "session"]
                        .contains(&key.as_str())
                        || key == "token")
                        && value.is_object();
                    let sensitive = !container
                        && (secret
                            || [
                                "token",
                                "secret",
                                "password",
                                "cookie",
                                "api_key",
                                "apikey",
                                "authorization",
                                "session",
                            ]
                            .iter()
                            .any(|part| key.contains(part)));
                    self.walk(value, sensitive, depth + 1)?;
                }
            }
            Value::Array(values) => {
                for value in values {
                    self.walk(value, secret, depth + 1)?;
                }
            }
            Value::String(value) if secret => {
                self.add(value)?;
            }
            _ => {}
        }
        Ok(())
    }

    pub fn contains(&self, value: &str) -> bool {
        self.secrets.iter().any(|secret| value.contains(secret))
    }

    pub fn text(&self, value: &str, limit: usize) -> String {
        let mut output = value.to_string();
        for secret in &self.secrets {
            output = output.replace(secret, "[已隐藏]");
        }
        output
            .chars()
            .filter(|character| !character.is_control())
            .take(limit)
            .collect()
    }

    pub fn field(&self, value: &Value, key: &str, limit: usize) -> Option<String> {
        crate::util::text(value, key)
            .map(|text| self.text(text, limit))
            .filter(|text| !text.is_empty())
    }
}
