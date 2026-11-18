//! 千问办公设备令牌提前一小时续期，target=c；过期太久可能不能续，不能等失败才维护。
//! 锁内比较原始 secret 后回写，未知结果持久保留，不在超时或重启后重用原刷新令牌。
use super::{
    account::{self, Snapshot},
    auth, client,
    credentials::Credentials,
    endpoints, util,
};
use agent2api_wasm_sdk::{
    json,
    state::{self, Scope},
    AccountLock, Error, Result, Value,
};

const CONTROL: &str = "refresh-control";
fn control() -> Result<state::Snapshot> {
    state::get(Scope::Account, CONTROL, 1)
}
fn put(before: &state::Snapshot, value: Value) -> Result<bool> {
    state::compare_exchange(Scope::Account, CONTROL, 1, before.revision, value, 0)
}
pub fn due() -> Result<bool> {
    let current = account::read()?;
    Ok(current.credentials.is_biz != Some(true) && current.credentials.expiring()?
        && agent2api_wasm_sdk::refresh_guard::available("credential-refresh", current.credentials.oauth_refresh())?)
}
pub fn ensure(force: bool) -> Result<Snapshot> {
    let initial = account::ready()?;
    if !force && !initial.credentials.expiring()? {
        usable(&initial.credentials)?;
        return Ok(initial);
    }
    let _lock = AccountLock::acquire()?;
    let current = account::read()?;
    if !account::same(&initial, &current) {
        return compatible(&initial, current);
    }
    if !force && !current.credentials.expiring()? {
        usable(&current.credentials)?;
        return Ok(current);
    }
    if !current.credentials.can_refresh() {
        return Err(Error::http(401, "千问办公没有刷新凭据，请重新授权"));
    }
    if current.credentials.is_biz == Some(true) {
        return Err(Error::http(409, "千问办公企业身份不能按个人设备凭据续期，请更新完整企业登录记录；未消费刷新令牌")
            .with_code("identity_changed"));
    }
    let stamp = util::hash(
        &json!([
            current.credentials.oauth_refresh(),
            current.credentials.binding()
        ])
        .to_string(),
    );
    let before = control()?;
    if !before.compatible {
        return Err(Error::http(
            409,
            "千问办公续期记忆格式不兼容，未重复使用刷新令牌",
        ));
    }
    if before.value["stamp"] == stamp {
        return pending(&current, force);
    }
    let marker = util::uuid()?;
    if !put(
        &before,
        json!({ "stamp": stamp, "marker": marker, "at": util::now()? }),
    )? {
        return Err(Error::http(409, "千问办公续期状态已变化"));
    }
    let reserved = control()?;
    if reserved.value["marker"] != marker {
        return Err(Error::http(409, "千问办公续期预约已变化，未消耗刷新凭据"));
    }
    account::unchanged(&current)?;
    let reservation = agent2api_wasm_sdk::refresh_guard::Reservation::acquire(
        "credential-refresh", current.credentials.oauth_refresh())?;
    let next = match upstream(&current.credentials) {
        Ok(next) => next,
        Err(error) => {
            let now = util::now()?;
            if !force
                && !util::terminal(&error)
                && error.status != Some(409)
                && current.credentials.expires.is_some_and(|at| at > now)
            {
                account::unchanged(&current)?;
                return Ok(current);
            }
            return Err(error);
        }
    };
    if !next.same_account(&current.credentials) {
        return Err(account::changed());
    }
    let completed_at = util::now()?;
    if next.expires.is_some_and(|at| at <= completed_at) {
        return Err(Error::http(502, "千问办公续期返回了已过期凭据"));
    }
    if next.access == current.credentials.access && next.refresh == current.credentials.refresh {
        return Err(Error::http(409, "千问办公续期仍返回旧凭据，未记为刷新成功"));
    }
    let latest = account::read()?;
    if !account::same(&current, &latest) {
        return compatible(&current, latest);
    }
    if !account::save(&current, &next)? {
        return compatible(&current, account::read()?);
    }
    let finished = account::read()?;
    if !finished.credentials.same_account(&next)
        || finished.credentials.access != next.access || finished.credentials.refresh != next.refresh {
        return Err(Error::http(409, "千问办公续期保存结果未确认，未清除旧凭据预约"));
    }
    reservation.complete()?;
    if !put(&reserved, Value::Null)? {
        return Err(Error::http(409, "千问办公续期保存后的状态未确认，不会重复使用旧刷新令牌"));
    }
    compatible(&current, finished)
}
fn usable(credentials: &Credentials) -> Result<()> {
    let now = util::now()?;
    if credentials.expires.is_some_and(|at| at <= now) {
        return Err(Error::http(401, "千问办公访问凭据已过期，请重新授权")
            .with_code("qwenwork_auth_expired"));
    }
    Ok(())
}
fn pending(current: &Snapshot, force: bool) -> Result<Snapshot> {
    let now = util::now()?;
    if !force && current.credentials.expires.is_some_and(|at| at > now) {
        account::unchanged(current)?;
        return Ok(current.clone());
    }
    Err(Error::http(
        409,
        "千问办公上次续期结果未确认，请重新授权；未重复使用旧刷新令牌",
    ))
}
fn compatible(previous: &Snapshot, current: Snapshot) -> Result<Snapshot> {
    if previous.record["id"] != current.record["id"]
        || previous.record["provider"] != current.record["provider"]
        || previous.record["identity"] != current.record["identity"]
        || previous.record["desktop"] != current.record["desktop"]
        || !previous.credentials.same_account(&current.credentials)
        || previous.credentials.access != current.credentials.access
            && previous.credentials.oauth_refresh() == current.credentials.oauth_refresh()
    {
        return Err(account::changed());
    }
    if account::same(previous, &current) {
        return Err(Error::http(409, "千问办公续期结果未保存，请重试"));
    }
    usable(&current.credentials)?;
    Ok(current)
}
fn upstream(previous: &Credentials) -> Result<Credentials> {
    let refresh = previous.oauth_refresh();
    if refresh.is_empty() {
        return Err(Error::http(401, "千问办公缺少刷新令牌"));
    }
    if previous.is_biz.is_none() {
        let mut checked = previous.clone();
        auth::profile(&mut checked)?;
        if checked.is_biz == Some(true) {
            return Err(Error::http(
                409,
                "旧千问办公企业账号缺少额度池身份，请重新导入完整记录；未消费刷新令牌",
            )
            .with_code("identity_changed"));
        }
    }
    let body = json!({ "refresh_token": refresh, "target": endpoints::REFRESH_TARGET,
        "client_id": endpoints::CLIENT_ID, "redirect_uri": endpoints::REDIRECT_URI });
    let reply = client::request(
        "POST",
        endpoints::DEVICE_REFRESH,
        client::refresh_headers()?,
        Some(&body),
        "令牌续期",
    )?;
    if matches!(reply.status, 401 | 403) {
        return Err(Error::http(
            401,
            "千问办公刷新凭据已失效，请重新授权；设备令牌过期太久可能无法续期",
        ));
    }
    let data = client::accepted(reply, "令牌续期")?;
    let data = client::unwrap_data(&data);
    // 设备响应 id 是会话号，不拿来替代 user_id。
    let user = util::text(&data, &["user_id", "uid", "userId"]);
    if !user.is_empty() && user != previous.user {
        return Err(account::changed());
    }
    let access = util::secret(
        &data,
        &["device_token", "deviceToken", "token", "access_token"],
    )?;
    let new_refresh = util::secret(&data, &["refresh_token", "refreshToken"])?;
    if access.is_empty()
        || new_refresh.is_empty()
        || access.contains('|')
        || new_refresh.contains('|')
        || access.starts_with("jt-")
        || new_refresh.starts_with("jrt-")
    {
        return Err(Error::http(502, "千问办公续期未返回有效设备凭据"));
    }
    let mut next = previous.clone();
    next.access = access;
    next.refresh = format!("{}|{}|{}", new_refresh, previous.user, previous.machine);
    next.expires =
        util::token_expiry(data, &next.access)?.or(Some(util::now()? + endpoints::DEVICE_LIFETIME));
    next.complete()?;
    Ok(next)
}
