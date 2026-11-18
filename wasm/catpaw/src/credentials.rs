//! 固定账号的凭据快照；desktop-auth 仅表示实时来源，绝不变成一次性 token。
//! 基于原生 accounts/credentials 的 uid||loginName 规则，不读取磁盘或环境变量。

use agent2api_wasm_sdk::{self as sdk, Error, Result, Value};

use crate::{endpoints, errors, gates, util};

pub struct Credentials {
    pub token: String,
    pub uid: String,
    pub login_name: String,
    pub balance_token: Option<String>,
    pub ignored_balance: bool,
}

impl Credentials {
    pub fn parse(value: &Value) -> Result<Self> {
        let value = payload(value)?;
        if is_desktop(&value) { return Err(gates::desktop()); }
        let auth = &value["auth"];
        for kind in [auth.get("loginType"), value.get("loginType")].into_iter().flatten().filter(|kind| !kind.is_null()) {
            let kind = kind.as_str().ok_or_else(|| errors::invalid("CatPaw 登录类型必须是文本"))?;
            if !kind.trim().is_empty() && !kind.trim().eq_ignore_ascii_case("passport") {
                return Err(errors::invalid("当前 CatPaw 基础包只接受 passport 登录凭据"));
            }
        }
        let token = first(&[value.get("token"), value.get("accessToken"), value.get("access_token"),
            value.get("auth_token"), auth.get("accessToken"), auth.get("token"), auth.get("access_token")])?;
        let token = token.map(|token| token_field(&token)).transpose()?.unwrap_or_default();
        let account = &value["account"];
        let uid = first(&[value.get("uid"), value.get("userId"), account.get("uid"), account.get("userId")])?;
        let login = first(&[value.get("loginName"), account.get("loginName")])?;
        // 旧原生账号 id 就是 uid/loginName；宿主生成的 catpaw-account-* 主键不是 user-uid。
        let legacy = util::text(&value, &["id"]);
        let legacy = (!legacy.starts_with("catpaw-account-") && !legacy.is_empty()).then_some(legacy);
        // 与登录名相同的旧 id 不是新增 uid；不同 id 仍保留原旧账号兜底身份。
        // 避免登录名账号下一次读取时自行变成 uid 账号。
        let uid = uid.or_else(|| {
            if legacy.as_deref() == login.as_deref() { None } else { legacy }
        }).unwrap_or_default();
        let login_name = login.unwrap_or_default();
        let uid = optional_header(&uid, "uid", endpoints::MAX_USER_CHARS)?;
        let login_name = optional_header(&login_name, "登录名", endpoints::MAX_USER_CHARS)?;
        let (balance_token, ignored_balance) = balance(&value);
        Ok(Self { token, uid, login_name, balance_token, ignored_balance })
    }

    pub fn user(&self) -> &str { if self.uid.is_empty() { &self.login_name } else { &self.uid } }
    pub fn identity(&self) -> Result<String> {
        let identity = self.user();
        if identity.is_empty() { return Err(errors::invalid("CatPaw 登录态缺少 uid 或登录名，无法识别账号")); }
        if identity == endpoints::DESKTOP_ID || identity.len() > 512
            || errors::contains_secret(identity, &self.secrets()) {
            return Err(errors::invalid("CatPaw 账号身份无效或包含凭据，未修改业务身份"));
        }
        Ok(identity.to_string())
    }
    pub fn require_main(&self) -> Result<()> {
        if self.token.is_empty() { return Err(errors::invalid("CatPaw 缺少登录令牌，请提供 token 或 accessToken")); }
        self.identity().map(|_| ())
    }
    pub fn balance_credential(&self) -> Result<&str> {
        if !self.token.is_empty() { return Ok(&self.token); }
        self.balance_token.as_deref().ok_or_else(|| Error::http(503,
            "CatPaw 账号没有可用积分查询凭据，请重新登录后导入").with_code("catpaw_credentials_missing"))
    }
    pub fn secrets(&self) -> Vec<&str> {
        let mut values = vec![self.token.as_str()];
        if let Some(token) = &self.balance_token { values.push(token); }
        values
    }
}

/// 宿主已经绑定账号与出口；这里只读当前账号，不枚举、不回落另一账号。
pub fn current() -> Result<Credentials> {
    let record = sdk::account().map_err(errors::host)?;
    if record["provider"].as_str() != Some(endpoints::PROVIDER) {
        return Err(Error::http(403, "当前账号不属于 CatPaw").with_code("forbidden"));
    }
    if sdk::account_disabled(&record) {
        return Err(Error::http(403, "CatPaw 账号已停用").with_code("forbidden"));
    }
    if record["public"]["desktop"] == true || record["id"] == endpoints::DESKTOP_ID
        || is_desktop(&record) || is_desktop(&record["secret"]) {
        return crate::desktop::read();
    }
    let value = payload(&record["secret"])?;
    // 仅兼容旧主键中存着用户标识的记录；不把新宿主散列主键伪装成 uid。
    let mut value = value;
    if util::text(&value, &["uid", "userId", "loginName", "id"]).is_empty()
        && util::text(&value["account"], &["uid", "userId", "loginName"]).is_empty() {
        if let Some(id) = record["id"].as_str().filter(|id| !id.starts_with("catpaw-account-")) {
            value["id"] = Value::String(id.to_string());
        }
    }
    Credentials::parse(&value)
}

pub fn check_current(before: &Credentials) -> Result<()> {
    let after = current()?;
    if before.token != after.token || before.uid != after.uid || before.login_name != after.login_name
        || before.balance_token != after.balance_token {
        return Err(Error::http(409, "CatPaw 账号凭据或本机登录用户在读取期间已变化，请重新发起；未返回上一账号的数据")
            .with_code("catpaw_account_changed"));
    }
    Ok(())
}

pub fn own_provider(value: &Value) -> Result<()> {
    if let Some(provider) = value.get("provider").filter(|value| !value.is_null()) {
        if provider.as_str() != Some(endpoints::PROVIDER) {
            return Err(errors::invalid("所提供的账号不属于 CatPaw"));
        }
    }
    Ok(())
}

pub fn is_desktop(value: &Value) -> bool {
    value["desktop"] == true || value["id"] == endpoints::DESKTOP_ID
        || value["identity"] == endpoints::DESKTOP_ID || value["pluginIdentity"] == endpoints::DESKTOP_ID
        || value["source"] == "desktop" || value["source"] == "desktop-auth"
        || value["source"] == "desktop-live" || value["source"]["kind"] == "desktop"
        || value["public"]["desktop"] == true || value["pluginPublic"]["desktop"] == true
}

/// 只展开显式导入文本/标准宿主信封；含实时来源标记时先拒绝，不能洗成快照。
pub fn payload(value: &Value) -> Result<Value> {
    let mut value = value.clone();
    for _ in 0..5 {
        if let Some(text) = value.as_str() { value = util::json_object(text)?; continue; }
        if !value.is_object() { return Err(errors::invalid("CatPaw 凭据必须是单账号 JSON 对象")); }
        if value.to_string().len() > endpoints::MAX_INPUT_BYTES { return Err(errors::invalid("CatPaw 凭据对象超过大小限制")); }
        own_provider(&value)?;
        if is_desktop(&value) { return Err(gates::desktop()); }
        if value.get("pluginSecret").is_some_and(Value::is_object) || value.get("secret").is_some_and(Value::is_object) {
            let mut inner = value.get("pluginSecret").filter(|value| value.is_object())
                .or_else(|| value.get("secret").filter(|value| value.is_object())).cloned()
                .ok_or_else(|| errors::invalid("CatPaw 账号信封无效"))?;
            for key in ["uid", "loginName", "name", "id"] {
                if inner.get(key).is_none() {
                    if let Some(source) = value.get(key) { inner[key] = source.clone(); }
                }
            }
            value = inner;
            continue;
        }
        let nested = ["token", "accessToken", "access_token", "auth_token", "credentialJson"]
            .iter().find_map(|key| value.get(*key).and_then(Value::as_str)
                .filter(|text| text.trim_start().starts_with('{')));
        if let Some(text) = nested {
            let mut inner = util::json_object(text)?;
            for key in ["uid", "loginName", "name", "balanceToken"] {
                if inner.get(key).is_none() {
                    if let Some(source) = value.get(key) { inner[key] = source.clone(); }
                }
            }
            value = inner;
            continue;
        }
        for key in ["auth", "account"] {
            if value.get(key).is_some_and(|value| !value.is_null() && !value.is_object()) {
                return Err(errors::invalid("CatPaw auth/account 字段必须为对象"));
            }
        }
        return Ok(value);
    }
    Err(errors::invalid("CatPaw 凭据封装层数过多"))
}

fn first(values: &[Option<&Value>]) -> Result<Option<String>> {
    for value in values.iter().flatten().filter(|value| !value.is_null()) {
        let value = util::scalar(Some(value)).ok_or_else(|| errors::invalid("CatPaw 凭据及用户标识必须是文本或数字"))?;
        if !value.trim().is_empty() { return Ok(Some(value.trim().to_string())); }
    }
    Ok(None)
}

pub fn token_field(raw: &str) -> Result<String> {
    let raw = raw.trim();
    let raw = match raw.get(..7) {
        Some(prefix) if prefix.eq_ignore_ascii_case("bearer ") => raw[7..].trim(),
        _ => raw,
    };
    header(raw, "令牌", endpoints::MAX_TOKEN_CHARS)
}

fn header(raw: &str, field: &str, max: usize) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.chars().count() > max || raw.chars().any(char::is_control) || raw.contains(';') {
        return Err(errors::invalid(&format!("CatPaw {field}为空、过长或包含非法头字符")));
    }
    Ok(raw.to_string())
}

fn optional_header(raw: &str, field: &str, max: usize) -> Result<String> {
    if raw.is_empty() { Ok(String::new()) } else { header(raw, field, max) }
}

fn balance(value: &Value) -> (Option<String>, bool) {
    let mut ignored = false;
    for candidate in [value.get("balanceToken"), value.get("token2"), value["auth"].get("token2"),
        value["balanceCookie"].get("token2")].into_iter().flatten().filter(|value| !value.is_null()) {
        let Some(raw) = util::scalar(Some(candidate)) else { ignored = true; continue; };
        if raw.trim().is_empty() { continue; }
        match token_field(&raw) {
            Ok(token) => return (Some(token), ignored),
            Err(_) => ignored = true,
        }
    }
    (None, ignored)
}
