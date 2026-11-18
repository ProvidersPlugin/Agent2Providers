//! 不透明双令牌；保留 sgcookie 和账号级设备指纹，不读取客户端文件或系统环境。

use crate::{endpoints, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

#[derive(Clone)]
pub struct Credentials {
    pub access: String,
    pub refresh: String,
    pub expires: Option<i64>,
    pub user: String,
    pub email: String,
    pub name: String,
    pub device: String,
    pub sgcookie: String,
}

pub fn international(value: &Value) -> Result<()> {
    if value
        .get("provider")
        .and_then(Value::as_str)
        .is_some_and(|provider| provider != endpoints::ID)
    {
        return Err(Error::http(400, "凭据所属提供商不是 Accio 国际版"));
    }
    // 身份以独立 provider 为准；不能把明确的国内凭证悄悄重标成国际。
    for key in [
        "mode",
        "edition",
        "region",
        "packageRegion",
        "package_region",
    ] {
        match value.get(key) {
            None | Some(Value::Null) => {}
            Some(Value::String(text))
                if matches!(
                    text.trim().to_ascii_lowercase().as_str(),
                    "" | "global" | "intl"
                ) => {}
            _ => {
                return Err(Error::http(
                    400,
                    "此插件只接受 Accio 国际版凭证，国内账号请使用 accio-cn",
                ))
            }
        }
    }
    Ok(())
}

pub fn token(value: &Value, keys: &[&str]) -> Result<String> {
    let mut text = keys
        .iter()
        .filter_map(|key| value[*key].as_str())
        .map(str::trim)
        .find(|text| !text.is_empty())
        .unwrap_or("");
    if text
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Bearer "))
    {
        text = text[7..].trim();
    }
    if text.len() > 8192 || text.chars().any(char::is_control) {
        return Err(Error::http(400, "Accio 凭证过长或包含控制字符"));
    }
    Ok(text.to_string())
}

fn profile_text(value: &Value, keys: &[&str], limit: usize) -> String {
    util::text(value, keys)
        .chars()
        .filter(|character| !character.is_control())
        .take(limit)
        .collect()
}

pub fn header_safe(value: &str) -> bool {
    !value.is_empty() && value.len() <= 4096 && value.bytes().all(|byte| (32..=126).contains(&byte))
}

pub fn sgcookie(value: &Value) -> String {
    let cookie = util::text(value, &["sgcookie", "sgCookie"]);
    if header_safe(&cookie) {
        cookie
    } else {
        String::new()
    }
}

impl Credentials {
    pub fn parse(value: &Value) -> Result<Self> {
        if !value.is_object() {
            return Err(Error::http(400, "Accio 凭证必须是 JSON 对象"));
        }
        international(value)?;
        let access = token(value, &["accessToken", "access_token", "token"])?;
        if access.is_empty() {
            return Err(Error::http(400, "缺少 Accio 国际版访问令牌"));
        }
        let user = util::text(value, &["userId", "user_id", "uid"]);
        if user.len() > 480 || user.chars().any(char::is_control) {
            return Err(Error::http(400, "Accio 账号标识无效，未截断或重写身份"));
        }
        Ok(Self {
            access,
            refresh: token(value, &["refreshToken", "refresh_token"])?,
            expires: ["expiresAt", "expires_at", "expires"]
                .iter()
                .find_map(|key| util::timestamp(value.get(*key))),
            user,
            email: profile_text(value, &["email"], 320),
            name: profile_text(value, &["nickname", "name"], 100),
            device: util::text(value, &["deviceId", "device_id", "utdid"]),
            sgcookie: sgcookie(value),
        })
    }

    pub fn ensure_device(&mut self, new_account: bool) -> Result<()> {
        if !header_safe(&self.device) {
            self.device = if new_account {
                format!("desktop-{}", util::uuid()?.replace('-', ""))
            } else {
                format!(
                    "desktop-{}",
                    util::md5(if self.user.is_empty() {
                        &self.access
                    } else {
                        &self.user
                    })
                )
            };
        }
        Ok(())
    }

    pub fn expiring(&self, now: i64) -> bool {
        !self.refresh.is_empty()
            && self
                .expires
                .is_some_and(|expires| expires <= now.saturating_add(300_000))
    }

    pub fn advanced_from(&self, previous: &Self) -> bool {
        self.access != previous.access
            || self.refresh != previous.refresh
            || self.expires > previous.expires
    }

    pub fn value(&self) -> Value {
        let optional = |value: &str| {
            if value.is_empty() {
                Value::Null
            } else {
                json!(value)
            }
        };
        json!({ "mode": endpoints::EDITION, "accessToken": self.access, "refreshToken": optional(&self.refresh),
            "expiresAt": self.expires, "userId": optional(&self.user), "email": optional(&self.email),
            "nickname": optional(&self.name), "deviceId": optional(&self.device), "sgcookie": optional(&self.sgcookie) })
    }

    pub fn profile(&mut self, value: &Value) {
        let value = value
            .get("user")
            .filter(|user| user.is_object())
            .map(|user| {
                let mut merged = value.clone();
                for keys in [
                    &["id", "userId", "user_id", "uid"][..],
                    &["nickname", "name", "displayName", "userName"][..],
                    &["email", "emailAddress"][..],
                    &["deviceId", "device_id", "utdid"][..],
                ] {
                    if !util::text(user, keys).is_empty() {
                        if let Some(fields) = merged.as_object_mut() {
                            for key in keys {
                                fields.remove(*key);
                            }
                        }
                    }
                }
                for (key, entry) in user.as_object().into_iter().flatten() {
                    if !entry.is_null()
                        && !entry.as_str().is_some_and(|text| text.trim().is_empty())
                    {
                        merged[key] = entry.clone();
                    }
                }
                merged
            })
            .unwrap_or_else(|| value.clone());
        let user = util::text(&value, &["id", "userId", "user_id", "uid"]);
        if !user.is_empty()
            && user.len() <= 480
            && !user.chars().any(char::is_control)
            && !self
                .secrets()
                .iter()
                .filter(|secret| !secret.is_empty())
                .any(|secret| user.contains(*secret))
        {
            self.user = user;
        }
        let name = profile_text(
            &value,
            &["nickname", "name", "displayName", "userName"],
            100,
        );
        if !name.is_empty() {
            self.name = name;
        }
        let email = profile_text(&value, &["email", "emailAddress"], 320);
        if !email.is_empty() {
            self.email = email;
        }
        let device = util::text(&value, &["deviceId", "device_id", "utdid"]);
        if self.device.is_empty() && header_safe(&device) {
            self.device = device;
        }
    }

    pub fn secrets(&self) -> [&str; 3] {
        [&self.access, &self.refresh, &self.sgcookie]
    }
}
