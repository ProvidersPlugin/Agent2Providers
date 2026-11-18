//! 凭证兼容现存 API key / 社交 OAuth / IDC / 外部 IdP 与显式本机引用。
//! 本机来源不被改造成 token 快照；没有真实 helper 时任何业务操作均明确拒绝。

use crate::{endpoints, errors, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

#[derive(Clone, Default)]
pub struct Credentials {
    pub method: String,
    pub access: String,
    pub refresh: String,
    pub expires: Option<i64>,
    pub region: String,
    pub profile: String,
    pub client_id: String,
    pub client_secret: String,
    pub token_url: String,
    pub login_provider: String,
    pub uid: String,
    pub account_id: String,
    pub plan: String,
    pub source: String,
    pub name: String,
    pub identity_seed: String,
}
pub fn token(value: &Value, keys: &[&str]) -> Result<String> {
    let text = util::text(value, keys);
    let text = if text
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Bearer "))
    {
        text[7..].trim().to_string()
    } else {
        text
    };
    if text.len() > 8192 || text.chars().any(char::is_control) {
        return Err(Error::http(400, "Kiro 凭据过长或包含控制字符"));
    }
    Ok(text)
}
fn oauth_response_error() -> Error {
    Error::http(
        502,
        "Kiro授权响应的凭据或有效期缺失、无效或冲突，未接受本次凭据",
    )
    .with_code("kiro_token_response_invalid")
}

/// 只用于真实换码/续期响应；不沿用导入时trim/Bearer/首个别名优先的宽松规则。
pub(crate) fn oauth_token(value: &Value, keys: &[&str], required: bool) -> Result<String> {
    if !value.is_object() {
        return Err(oauth_response_error());
    }
    let mut found: Option<&str> = None;
    for key in keys {
        if let Some(raw) = value.get(*key) {
            let text = raw
                .as_str()
                .filter(|text| {
                    !text.is_empty()
                        && text.len() <= 8192
                        && !text.chars().any(|ch| ch.is_control() || ch.is_whitespace())
                })
                .ok_or_else(oauth_response_error)?;
            if found.is_some_and(|old| old != text) {
                return Err(oauth_response_error());
            }
            found = Some(text);
        }
    }
    match found {
        Some(text) => Ok(text.to_string()),
        None if !required => Ok(String::new()),
        None => Err(oauth_response_error()),
    }
}

/// 未提供期限或仅null保持未知；已提供的两个别名必须同为有效正整数秒。
/// 只换算真实读数，不补一小时，也不继承新token以前的期限。
pub(crate) fn oauth_expiry(value: &Value) -> Result<Option<i64>> {
    if !value.is_object() {
        return Err(oauth_response_error());
    }
    let mut seconds = None;
    let mut unknown = false;
    for key in ["expiresIn", "expires_in"] {
        if let Some(raw) = value.get(key) {
            if raw.is_null() {
                unknown = true;
                continue;
            }
            let next = raw
                .as_i64()
                .filter(|seconds| *seconds > 0)
                .ok_or_else(oauth_response_error)?;
            if seconds.is_some_and(|old| old != next) {
                return Err(oauth_response_error());
            }
            seconds = Some(next);
        }
    }
    if unknown && seconds.is_some() {
        return Err(oauth_response_error());
    }
    let Some(seconds) = seconds else {
        return Ok(None);
    };
    let expires = util::now()?
        .checked_add(seconds.checked_mul(1000).ok_or_else(oauth_response_error)?)
        .filter(|expires| {
            *expires > 0
                && *expires < (1_i64 << 53)
                && chrono::DateTime::from_timestamp_millis(*expires).is_some()
        })
        .ok_or_else(oauth_response_error)?;
    Ok(Some(expires))
}

fn field(value: &Value, keys: &[&str], limit: usize) -> Result<String> {
    let text = util::text(value, keys);
    if text.len() > limit || text.chars().any(char::is_control) {
        return Err(Error::http(400, "Kiro 账号字段格式无效"));
    }
    Ok(text)
}
fn aws_region(region: &str) -> bool {
    let parts: Vec<_> = region.split('-').collect();
    let Some((number, words)) = parts.split_last() else {
        return false;
    };
    (9..=32).contains(&region.len())
        && words.len() >= 2
        && words[0].len() == 2
        && words
            .iter()
            .all(|word| !word.is_empty() && word.bytes().all(|byte| byte.is_ascii_lowercase()))
        && (1..=2).contains(&number.len())
        && number.bytes().all(|byte| byte.is_ascii_digit())
}
impl Credentials {
    pub fn parse(value: &Value) -> Result<Self> {
        if !value.is_object() {
            return Err(Error::http(400, "Kiro 凭据必须是 JSON 对象"));
        }
        let access = token(
            value,
            &[
                "access",
                "accessToken",
                "access_token",
                "token",
                "apiKey",
                "key",
            ],
        )?;
        let client_id = field(value, &["clientId", "client_id"], 4096)?;
        let method = util::text(value, &["method", "authMethod"]).to_ascii_lowercase();
        let provider = util::text(value, &["provider"]).to_ascii_lowercase();
        let method = match method.as_str() {
            "external_idp" | "externalidp" => "external-idp".to_string(),
            "api_key" => "apikey".to_string(),
            "oidc" | "odic" => "idc".to_string(),
            "" => if access.starts_with("ksk_") || value.get("apiKey").is_some() {
                "apikey"
            } else if matches!(
                provider.as_str(),
                "externalidp" | "external_idp" | "external-idp"
            ) {
                "external-idp"
            } else if matches!(provider.as_str(), "google" | "github") {
                "social"
            } else if !client_id.is_empty() {
                "idc"
            } else {
                "social"
            }
            .to_string(),
            _ => method,
        };
        if !["apikey", "social", "idc", "external-idp"].contains(&method.as_str()) {
            return Err(Error::http(400, "Kiro 授权类型无效"));
        }
        let source = field(value, &["source"], 80)?;
        if !source.is_empty() && !["kiro-cli", "kiro-ide", "kiro"].contains(&source.as_str()) {
            return Err(Error::http(400, "Kiro 本机来源无效"));
        }
        if source.is_empty() && access.is_empty() {
            return Err(Error::http(400, "缺少 Kiro 访问凭据"));
        }
        let region = field(value, &["region"], 32)?;
        let region = if region.is_empty() {
            "us-east-1".into()
        } else {
            region
        };
        if !aws_region(&region) {
            return Err(Error::http(400, "Kiro AWS 地区格式无效"));
        }
        let mut login_provider =
            field(value, &["loginProvider", "login_provider"], 40)?.to_ascii_lowercase();
        let provider = match provider.as_str() {
            "externalidp" | "external_idp" => "external-idp".to_string(),
            _ => provider,
        };
        if !provider.is_empty() && provider != endpoints::ID {
            if [
                "google",
                "github",
                "builderid",
                "enterprise",
                "internal",
                "external-idp",
            ]
            .contains(&provider.as_str())
            {
                if login_provider.is_empty() {
                    login_provider = provider;
                }
            } else {
                return Err(Error::http(400, "凭据不属于 Kiro"));
            }
        }
        let mut uid = field(value, &["uid"], 80)?;
        if uid.is_empty() {
            uid = login_provider.clone();
        }
        let result = Self {
            method,
            access,
            client_id,
            source,
            region,
            login_provider,
            uid,
            refresh: token(value, &["refresh", "refreshToken", "refresh_token"])?,
            expires: ["expires", "expiresAt", "expires_at"]
                .iter()
                .find_map(|key| util::timestamp(value.get(*key))),
            profile: field(value, &["profile", "profileArn", "profile_arn"], 2048)?,
            client_secret: token(value, &["clientSecret", "client_secret"])?,
            token_url: field(
                value,
                &["tokenURL", "tokenUrl", "tokenEndpoint", "token_endpoint"],
                8192,
            )?,
            account_id: field(value, &["accountId", "email"], 400)?,
            plan: field(value, &["plan", "subscriptionTitle"], 200)?,
            name: field(value, &["name"], 400)?,
            identity_seed: field(value, &["identitySeed"], 32)?,
        };
        if !result.identity_seed.is_empty()
            && (result.identity_seed.len() != 32
                || !result
                    .identity_seed
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(Error::http(400, "Kiro稳定身份种子无效"));
        }
        if !result.account_id.is_empty()
            && result
                .secrets()
                .iter()
                .filter(|secret| !secret.is_empty())
                .any(|secret| {
                    result
                        .account_id
                        .to_lowercase()
                        .contains(&secret.to_lowercase())
                })
        {
            return Err(Error::http(400, "Kiro账号标识不能包含凭据本体"));
        }
        if !result.profile.is_empty()
            && result
                .secrets()
                .iter()
                .filter(|secret| !secret.is_empty())
                .any(|secret| result.profile.contains(*secret))
        {
            return Err(Error::http(400, "Kiro profile不能包含凭据本体"));
        }
        result.runtime_region()?;
        if !result.token_url.is_empty() && !endpoints::https(&result.token_url) {
            return Err(Error::http(
                400,
                "Kiro 外部身份提供商地址必须是有效的 HTTPS 地址",
            ));
        }
        Ok(result)
    }
    pub fn api_key(&self) -> bool {
        self.method == "apikey"
    }
    pub fn local(&self) -> bool {
        !self.source.is_empty()
    }
    pub fn refreshable(&self) -> bool {
        !self.local()
            && !self.refresh.is_empty()
            && (self.method == "social"
                || self.method == "idc"
                    && !self.client_id.is_empty()
                    && !self.client_secret.is_empty()
                || self.method == "external-idp" && !self.client_id.is_empty() && !self.token_url.is_empty())
    }
    pub fn fresh(&self, now: i64, lead: i64) -> bool {
        self.api_key() || self.expires.is_none_or(|at| at > now.saturating_add(lead))
    }
    pub fn runtime_region(&self) -> Result<String> {
        endpoints::region_of(&self.profile, &self.region)
    }
    pub fn token_type(&self) -> Option<&'static str> {
        match self.method.as_str() {
            "apikey" => Some("API_KEY"),
            "external-idp" => Some("EXTERNAL_IDP"),
            _ => None,
        }
    }
    pub fn secrets(&self) -> [&str; 3] {
        [&self.access, &self.refresh, &self.client_secret]
    }
    pub fn pin_identity(&mut self) {
        if !self.api_key()
            && !self.local()
            && self.account_id.is_empty()
            && self.identity_seed.is_empty()
        {
            self.identity_seed = util::hash(&self.access, 16);
        }
    }
    pub fn advanced(&self, old: &Self) -> bool {
        self.access != old.access || self.refresh != old.refresh || self.expires > old.expires
    }
    pub fn value(&self) -> Value {
        json!({ "method": self.method, "access": self.access, "refresh": self.refresh, "expires": self.expires,
            "region": self.region, "profileArn": self.profile, "clientId": self.client_id, "clientSecret": self.client_secret,
            "tokenURL": self.token_url, "loginProvider": self.login_provider, "uid": self.uid,
            "accountId": self.account_id, "plan": self.plan, "source": self.source, "name": self.name,
            "identitySeed": self.identity_seed })
    }
    pub fn merge(&self, before: &Value) -> Value {
        let mut next = before.clone();
        if let Some(fields) = next.as_object_mut() {
            for key in [
                "accessToken",
                "access_token",
                "token",
                "apiKey",
                "key",
                "refreshToken",
                "refresh_token",
                "expiresAt",
                "expires_at",
                "profile",
                "profile_arn",
                "authMethod",
                "client_id",
                "client_secret",
            ] {
                fields.remove(key);
            }
            for (key, value) in self.value().as_object().into_iter().flatten() {
                fields.insert(key.clone(), value.clone());
            }
        }
        next
    }
}
pub fn local_unavailable() -> Error {
    Error::http(
        503,
        "Kiro 本机登录态不可读，请先在官方客户端登录，或改用 API key 和凭据 JSON 导入",
    )
    .with_code("kiro_local_helper_unavailable")
}
pub fn identity(c: &Credentials) -> Result<String> {
    if c.api_key() {
        return Ok(format!("kiro:key:{}", util::hash(&c.access, 8)));
    }
    if c.local() {
        return Ok(format!("kiro:{}", c.source));
    }
    if !c.identity_seed.is_empty() {
        return Ok(format!("kiro:token:{}", c.identity_seed));
    }
    if !c.account_id.is_empty() {
        let way = if c.uid.is_empty() { "own" } else { &c.uid };
        let id = format!("kiro:{}:{way}", c.account_id.to_lowercase());
        if id.len() > 512 || errors::redact(&id, &c.secrets()) != id {
            return Err(Error::http(400, "Kiro 账号身份无效或包含凭据"));
        }
        return Ok(id);
    }
    // 原生在此返回空身份。新导入需稳定非空，摘要只在缺实际邮箱时兜底；续期不改主键。
    Ok(format!("kiro:token:{}", util::hash(&c.access, 16)))
}
