//! 账号锁单飞 + 完整 secret CAS。401 强刷不再次消费并发任务已落盘的新凭据。
//! 缺 project 时与原生一样 best-effort 发现，不用全局单飞表。

use super::{account, credentials, oauth, project, util};
use agent2api_wasm_sdk::{AccountLock, Result, Value};

pub fn locked(record: Value, force: bool) -> Result<Value> {
    let secret = credentials::parse(&record["secret"])?;
    if !force && !credentials::needs_refresh(&secret, util::now()?) {
        credentials::token(&secret)?;
        return Ok(record);
    }
    let patch = oauth::refresh(&util::text(&secret, &["refreshToken"]))?;
    // 先提交新凭据，project 慢请求或失败不会丢掉已刷新成功的令牌。
    let record = account::update(record, &patch)?;
    project::locked(record, false)
}

pub fn ensure(force: bool, observed: Option<&Value>) -> Result<Value> {
    let before = account::current()?;
    let expected = observed.unwrap_or(&before["secret"]);
    let _lock = AccountLock::acquire()?;
    let latest = account::current()?;
    if latest["identity"] != before["identity"] {
        return Err(util::conflict());
    }
    if latest["secret"] != *expected && force {
        let secret = credentials::parse(&latest["secret"])?;
        if !credentials::needs_refresh(&secret, util::now()?) {
            credentials::token(&secret)?;
            return Ok(latest);
        }
    }
    locked(latest, force)
}

pub fn expiring() -> Result<bool> {
    let record = account::current()?;
    let secret = credentials::parse(&record["secret"])?;
    Ok(credentials::needs_refresh(&secret, util::now()?))
}
