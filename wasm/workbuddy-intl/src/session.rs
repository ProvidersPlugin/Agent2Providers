//! 一次性刷新先持久记录“已提交”，失败/取消后不重放旧refreshToken；新token暂存secret内。
//! 身份核对通过才CAS替换原凭据；展示字段变化可合并，uid/企业/授权身份不能跨号替换。
use super::{account, endpoints, headers, network, refresh_identity, util};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope, Snapshot},
    AccountLock, Error, Result, Value,
};

const KEY: &str = "refresh-journal";
#[derive(Clone)]
pub struct Session {
    pub record: Value,
    pub secret: Value,
}
impl Session {
    pub fn token(&self) -> &str {
        self.secret["accessToken"].as_str().unwrap_or("")
    }
    pub fn refresh(&self) -> &str {
        self.secret["refreshToken"].as_str().unwrap_or("")
    }
    pub fn endpoint(&self) -> Result<String> {
        endpoints::endpoint(&self.secret)
    }
    pub fn secrets(&self) -> [&str; 2] {
        [self.token(), self.refresh()]
    }
    pub fn stamp(&self) -> String {
        util::hash(&json!({ "auth": account::authority(&self.secret), "token": self.token(), "refresh": self.refresh() }).to_string())
    }
}
pub fn snapshot() -> Result<Session> {
    let record = account::record()?;
    let secret = account::parse(&record["secret"], record["secret"]["name"].as_str())?;
    Ok(Session { record, secret })
}
fn expiring(secret: &Value) -> bool {
    util::integer(&secret["expiresAt"])
        .filter(|v| *v > 0)
        .is_some_and(|v| v.saturating_sub(300_000) <= sdk::now())
}
fn refreshable(secret: &Value) -> bool {
    !util::text(secret, &["refreshToken"]).is_empty()
        && util::integer(&secret["refreshExpiresAt"]).is_none_or(|at| at <= 0 || at > sdk::now())
}
pub fn is_expiring() -> Result<bool> {
    let current = snapshot()?;
    Ok(refreshable(&current.secret) && expiring(&current.secret))
}
pub fn ensure(force: bool) -> Result<Session> {
    let current = snapshot()?;
    if !force && (!expiring(&current.secret) || current.refresh().is_empty()) {
        return Ok(current);
    }
    let lock = AccountLock::acquire()?;
    let latest = snapshot()?;
    if !account::same_auth(&current.secret, &latest.secret) {
        return Ok(latest);
    }
    let result = refresh_locked(latest, &lock);
    match result {
        // 兼容原临期失败仍可尝试未过期的accessToken，但未知刷新状态不再重发旧refresh。
        Err(_)
            if !force
                && util::integer(&current.secret["expiresAt"])
                    .is_some_and(|at| at > sdk::now()) =>
        {
            let latest = snapshot()?;
            if account::same_auth(&current.secret, &latest.secret) {
                Ok(latest)
            } else {
                Err(Error::http(409, "WorkBuddy续期期间凭据已变化，请重试"))
            }
        }
        result => result,
    }
}
pub fn ensure_locked(lock: &AccountLock) -> Result<Session> {
    let current = snapshot()?;
    if expiring(&current.secret) && !current.refresh().is_empty() {
        let previous = current.clone();
        match refresh_locked(current, lock) {
            Err(_)
                if util::integer(&previous.secret["expiresAt"])
                    .is_some_and(|at| at > sdk::now()) =>
            {
                let latest = snapshot()?;
                if account::same_auth(&previous.secret, &latest.secret) {
                    Ok(latest)
                } else {
                    Err(Error::http(409, "国际续期期间凭据已变化，请重试"))
                }
            }
            result => result,
        }
    } else {
        Ok(current)
    }
}
pub fn refresh_locked_after_rejection(previous: &Session, lock: &AccountLock) -> Result<Session> {
    let current = snapshot()?;
    if !account::same_auth(&previous.secret, &current.secret) {
        return Ok(current);
    }
    refresh_locked(current, lock)
}

fn journal() -> Result<Snapshot> {
    let value = state::get(Scope::Account, KEY, 1)?;
    if !value.compatible {
        return Err(Error::http(409, "WorkBuddy续期台账格式已变化，请重新登录"));
    }
    Ok(value)
}
fn save_journal(before: &Snapshot, value: Value) -> Result<()> {
    if !state::compare_exchange(Scope::Account, KEY, 1, before.revision, value, 0)? {
        return Err(Error::http(409, "WorkBuddy续期状态已更新"));
    }
    Ok(())
}
fn uncertain() -> Error {
    Error::http(
        409,
        "WorkBuddy上次续期结果未确认，未重放一次性刷新令牌，请重新登录",
    )
    .with_code("refresh_outcome_uncertain")
}
fn refresh_locked(current: Session, _lock: &AccountLock) -> Result<Session> {
    if current.refresh().is_empty() {
        return Err(Error::http(401, "WorkBuddy没有刷新凭据，请重新登录"));
    }
    let stamp = current.stamp();
    let previous = journal()?;
    let pending = &current.record["secret"]["workbuddyPendingRefresh"];
    let next = if pending["source"] == stamp && pending["auth"].is_object() {
        // 新凭据已在secret落库时优先恢复；不能因外部状态丢失重放旧refreshToken。
        pending["auth"].clone()
    } else if previous.value["source"] == stamp {
        return Err(uncertain());
    } else {
        if !refreshable(&current.secret) {
            return Err(Error::http(401, "WorkBuddy国际刷新凭据已到期，请重新登录"));
        }
        let mut request_headers = headers::refresh(&current.secret)?;
        let endpoint = current.endpoint()?;
        request_headers["X-Refresh-Token"] = json!(current.refresh());
        save_journal(
            &previous,
            json!({ "source": stamp, "phase": "submitted", "at": sdk::now() }),
        )?;
        let response = network::raw_timeout(
            "POST",
            &endpoint,
            &format!("{}/auth/token/refresh", endpoints::AUTH),
            request_headers,
            Some(&json!({})),
            30_000,
        )?;
        if matches!(response.status, 404 | 405) {
            return Err(Error::http(
                response.status,
                "WorkBuddy国际版续期接口不存在，请检查国际账号端点，而不是反复重登录",
            ));
        }
        let data = response.checked(&current.secrets())?;
        let mut next = data.clone();
        let token = util::strip(&util::text(&data, &["accessToken"]));
        if token.is_empty() || token == current.token() {
            return Err(Error::http(502, "WorkBuddy续期未返回新的访问凭据"));
        }
        next["accessToken"] = json!(token);
        let parsed = account::parse(
            &json!({ "account": current.secret, "auth": next, "endpoint": current.secret["endpoint"] }),
            current.secret["name"].as_str(),
        )?;
        for key in [
            "accessToken",
            "refreshToken",
            "domain",
            "tokenType",
            "expiresAt",
            "refreshExpiresAt",
        ] {
            if let Some(value) = parsed.get(key) {
                next[key] = value.clone();
            }
        }
        let now = sdk::now();
        for (absolute, relative) in [
            ("expiresAt", "expiresIn"),
            ("refreshExpiresAt", "refreshExpiresIn"),
        ] {
            if let Some(at) = util::expires(&data, absolute, relative, now) {
                next[absolute] = json!(at);
            }
        }
        let latest = snapshot()?;
        if !account::same_auth(&current.secret, &latest.secret) {
            return Ok(latest);
        }
        let mut secret = latest.record["secret"].clone();
        secret["workbuddyPendingRefresh"] = json!({ "source": stamp, "auth": next });
        if !account::save(&latest.record, &secret)? {
            return Err(uncertain());
        }
        let journal = journal()?;
        save_journal(
            &journal,
            json!({ "source": stamp, "phase": "ready", "at": now }),
        )?;
        next
    };
    let verified = refresh_identity::verify(&current.secret, &next)?;
    let mut candidate = current.secret.clone();
    for key in [
        "accessToken",
        "refreshToken",
        "expiresAt",
        "refreshExpiresAt",
    ] {
        if let Some(value) = next
            .get(key)
            .filter(|v| !v.is_null() && !v.as_str().is_some_and(|s| s.is_empty()))
        {
            candidate[key] = value.clone();
        }
    }
    for key in [
        "type",
        "oneidAccountId",
        "departmentFullName",
        "idp",
        "domain",
        "tokenType",
        "nickname",
        "enterpriseName",
    ] {
        if let Some(value) = verified.get(key) {
            candidate[key] = value.clone();
        }
    }
    let candidate = account::parse(&candidate, current.secret["name"].as_str())?;
    for _ in 0..3 {
        let latest = snapshot()?;
        if !account::same_auth(&current.secret, &latest.secret) {
            return Ok(latest);
        }
        let mut secret = latest.record["secret"].clone();
        for key in [
            "accessToken",
            "refreshToken",
            "expiresAt",
            "refreshExpiresAt",
        ] {
            if let Some(value) = candidate
                .get(key)
                .filter(|v| !v.is_null() && !v.as_str().is_some_and(|s| s.is_empty()))
            {
                secret[key] = value.clone();
            }
        }
        for key in [
            "type",
            "oneidAccountId",
            "departmentFullName",
            "idp",
            "domain",
            "tokenType",
            "nickname",
            "enterpriseName",
        ] {
            if let Some(value) = candidate.get(key) {
                secret[key] = value.clone();
            }
        }
        if let Some(fields) = secret.as_object_mut() {
            fields.remove("workbuddyPendingRefresh");
        }
        if account::save(&latest.record, &secret)? {
            let journal = journal()?;
            let _ = save_journal(
                &journal,
                json!({ "source": stamp, "phase": "committed", "at": sdk::now() }),
            );
            return snapshot();
        }
    }
    Err(Error::http(
        409,
        "WorkBuddy续期凭据已取得但未能完成保存，请重新发起续期或登录",
    ))
}
pub fn check(current: &Session) -> Result<()> {
    let latest = snapshot()?;
    let mut latest_authority = account::authority(&latest.secret);
    let mut original_authority = account::authority(&current.secret);
    for authority in [&mut latest_authority, &mut original_authority] {
        if let Some(domain) = authority["domain"].as_str() {
            authority["domain"] = json!(account::canonical_domain(domain));
        }
    }
    if latest_authority != original_authority {
        return Err(Error::http(
            409,
            "WorkBuddy账号身份已变化，本次结果未写给另一身份",
        ));
    }
    Ok(())
}
