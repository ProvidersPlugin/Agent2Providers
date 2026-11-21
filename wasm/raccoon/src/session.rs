//! 普通账号只读取当前宿主记录；本机引用实时读官方文件，续期写回需独立授权。

use super::{account, credentials::Credential, desktop, jwt, refresh_api, refresh_state};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

pub struct Session {
    pub record: Value,
    pub credential: Credential,
    pub source: Option<desktop::Source>,
}

pub fn snapshot() -> Result<Session> {
    let record = account::record()?;
    let secret = &record["secret"];
    if secret["desktop"] != true {
        return Ok(Session { credential: Credential::parse(secret)?, record, source: None });
    }
    let source = desktop::read(Some(secret))?;
    Ok(Session { credential: source.credential.clone(), record, source: Some(source) })
}

pub fn ensure(force: bool) -> Result<Session> {
    let before = snapshot()?;
    if before.source.is_some() && force {
        return refresh_desktop(before);
    }
    if jwt::iam(&before.credential.token) {
        if !force {
            jwt::validate_iam(&before.credential.token)?;
            return Ok(before);
        }
        let latest = snapshot()?;
        if latest.credential.token != before.credential.token {
            validate(&latest)?;
            return Ok(latest);
        }
        return Err(Error::http(401, "小浣熊 IAM 登录方式不支持 Token 续期；当前凭证有效时仍可使用，更新凭证请重新登录。")
            .with_code("raccoon_iam_relogin"));
    }
    if !force && !before.credential.expiring() {
        return Ok(before);
    }
    if before.source.is_some() && desktop::refresh_status()?["authorized"] != true {
        validate(&before)?;
        return Ok(before);
    }
    if before.source.is_none() && !force && !refresh_state::available(&before.credential)? {
        validate(&before)?;
        return Ok(before);
    }
    let _lock = AccountLock::acquire()?;
    let current = snapshot()?;
    if !before.credential.same(&current.credential) {
        validate(&current)?;
        return Ok(current);
    }
    renew(current)
}

/// 领取已持有账号锁时复用续期逻辑，不能再次申请嵌套账号锁。
pub fn ensure_locked(_guard: &AccountLock) -> Result<Session> {
    let current = snapshot()?;
    if jwt::iam(&current.credential.token) || !current.credential.expiring() {
        validate(&current)?;
        return Ok(current);
    }
    if current.source.is_some() && desktop::refresh_status()?["authorized"] != true {
        validate(&current)?;
        return Ok(current);
    }
    if current.source.is_none() && !refresh_state::available(&current.credential)? {
        validate(&current)?;
        return Ok(current);
    }
    renew(current)
}

fn validate(session: &Session) -> Result<()> {
    if jwt::iam(&session.credential.token) {
        return jwt::validate_iam(&session.credential.token);
    }
    if session.credential.expires.is_some_and(|at| at <= sdk::now()) {
        return Err(Error::http(401, "小浣熊登录态已过期，请重新登录；本机旧凭据须授权本机续期后才能自动刷新"));
    }
    Ok(())
}

fn refresh_desktop(before: Session) -> Result<Session> {
    let _lock = AccountLock::acquire()?;
    let mut current = snapshot()?;
    if !before.credential.same(&current.credential) {
        return sync_desktop(current);
    }
    if current.record["secret"]["sourceId"].as_str().is_none_or(str::is_empty) {
        let source = current.source.as_ref().ok_or_else(Error::unsupported)?;
        let reference = desktop::sync(&current.record["secret"], source);
        if !account::save(&current.record, &reference)? {
            return Err(Error::http(409, "小浣熊旧本机账号在关联期间已变化，请重新刷新"));
        }
        current = snapshot()?;
    }
    let status = desktop::refresh_status()?;
    if status["authorized"] == true && status["refreshable"] == true {
        return renew(current);
    }
    sync_desktop(current)
}

fn sync_desktop(current: Session) -> Result<Session> {
    validate(&current)?;
    let source = current.source.as_ref().ok_or_else(Error::unsupported)?;
    let reference = desktop::sync(&current.record["secret"], source);
    if !account::save(&current.record, &reference)? {
        return Err(Error::http(409, "小浣熊账号在同步期间已变化，请重新刷新"));
    }
    snapshot()
}

fn renew(current: Session) -> Result<Session> {
    if let Some(source) = &current.source {
        desktop::refresh(source)?;
        return sync_desktop(snapshot()?);
    }
    if !current.credential.can_refresh() {
        return Err(Error::http(401, "小浣熊登录态已失效且没有可用刷新令牌，请重新登录"));
    }
    let next = refresh_api::call(&current.credential)?;
    let mut secret = current.record["secret"].clone();
    secret["accessToken"] = json!(next.token);
    if let Some(fields) = secret.as_object_mut() {
        for key in ["access_token", "token", "auth_token", "refresh_token"] {
            fields.remove(key);
        }
        if jwt::iam(&next.token) || next.refresh.is_empty() || next.refresh == next.token {
            fields.remove("refreshToken");
        } else {
            fields.insert("refreshToken".into(), json!(next.refresh));
        }
    }
    if let Some(at) = next.expires {
        secret["expiresAt"] = json!(at);
    }
    secret["userId"] = json!(next.user);
    let saved = account::save(&current.record, &secret)?;
    let latest = snapshot()?;
    if saved && latest.credential.same(&next) {
        refresh_state::complete(&current.credential)?;
        return Ok(latest);
    }
    if !latest.credential.same(&current.credential) {
        validate(&latest)?;
        return Ok(latest);
    }
    Err(Error::http(409, "小浣熊续期结果未能落盘，请重新登录该账号"))
}

pub fn expiring(min_lead_ms: i64) -> Result<bool> {
    let session = snapshot()?;
    if !session.credential.can_refresh() || !session.credential.expiring_within(min_lead_ms) {
        return Ok(false);
    }
    if session.source.is_some() {
        return Ok(desktop::refresh_status()?["authorized"] == true);
    }
    refresh_state::available(&session.credential)
}
