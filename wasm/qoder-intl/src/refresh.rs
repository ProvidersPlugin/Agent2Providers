//! 锁内续期与原始 secret CAS；未知轮换结果持久保留，不因超时或重启重用刷新令牌。
//! PAT 重新兑换，jrt- 走作业刷新，其余设备刷新。尚未到期的访问凭据仍可只读使用。
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
fn stamp(credentials: &Credentials) -> String {
    util::hash(
        &json!([
            credentials.access,
            credentials.refresh,
            credentials.user,
            credentials.machine
        ])
        .to_string(),
    )
}
pub fn due(min_lead_ms: i64) -> Result<bool> {
    let current = account::read()?;
    Ok(current.credentials.expiring_within(min_lead_ms)?
        && (current.credentials.pat().is_some()
            || agent2api_wasm_sdk::refresh_guard::available("credential-refresh", current.credentials.oauth_refresh())?))
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
        return Err(Error::http(401, "Qoder 没有刷新凭据，请重新授权"));
    }
    let stamp = stamp(&current.credentials);
    let before = control()?;
    if !before.compatible || before.value["stamp"] == stamp {
        if !force && usable(&current.credentials).is_ok() {
            account::unchanged(&current)?;
            return Ok(current);
        }
        return Err(Error::http(
            409,
            "Qoder 上次续期结果未确认，请重新授权；未重复使用旧刷新令牌",
        )
        .with_code("qoder_refresh_unconfirmed"));
    }
    let marker = util::uuid()?;
    if !put(
        &before,
        json!({ "stamp": stamp, "marker": marker, "at": util::now()? }),
    )? {
        return Err(Error::http(409, "Qoder 续期状态已变化"));
    }
    let reserved = control()?;
    if reserved.value["marker"] != marker {
        return Err(Error::http(409, "Qoder 续期预约已变化，未消耗刷新凭据"));
    }
    account::unchanged(&current)?;
    let reservation = if current.credentials.pat().is_some() { None } else {
        Some(agent2api_wasm_sdk::refresh_guard::Reservation::acquire("credential-refresh", current.credentials.oauth_refresh())?)
    };
    let next = match upstream(&current.credentials) {
        Ok(next) => next,
        Err(error) => {
            let rejected = current.credentials.pat().is_some() && !util::terminal(&error)
                && error
                    .status
                    .is_some_and(|status| (400..500).contains(&status) && status != 408);
            if rejected {
                let _ = put(&reserved, Value::Null);
            } else if !force && !util::terminal(&error) && usable(&current.credentials).is_ok() {
                account::unchanged(&current)?;
                return Ok(current);
            }
            return Err(error);
        }
    };
    if next.user != current.credentials.user || next.machine != current.credentials.machine {
        return Err(account::changed());
    }
    usable(&next)?;
    if next.access == current.credentials.access
        || current.credentials.pat().is_none() && next.refresh == current.credentials.refresh
    {
        return Err(
            Error::http(409, "Qoder 续期未轮换完整凭据，结果待确认；请重新授权")
                .with_code("qoder_refresh_unconfirmed"),
        );
    }
    let latest = account::read()?;
    if !account::same(&current, &latest) {
        return compatible(&current, latest);
    }
    if !account::save(&current, &next)? {
        return compatible(&current, account::read()?);
    }
    let finished = account::read()?;
    if finished.credentials.binding() != next.binding()
        || finished.credentials.access != next.access || finished.credentials.refresh != next.refresh {
        return Err(Error::http(409, "Qoder 续期保存结果未确认，未清除旧凭据预约"));
    }
    if let Some(reservation) = reservation { reservation.complete()?; }
    if !put(&reserved, Value::Null)? {
        return Err(Error::http(409, "Qoder 续期保存后的状态未确认，不会重复使用旧刷新令牌"));
    }
    compatible(&current, finished)
}
fn usable(credentials: &Credentials) -> Result<()> {
    let now = util::now()?;
    if credentials.expires.is_some_and(|at| at <= now) {
        return Err(
            Error::http(401, "Qoder 访问凭据已过期且没有可用续期结果，请重新授权")
                .with_code("qoder_auth_expired"),
        );
    }
    Ok(())
}
fn compatible(previous: &Snapshot, current: Snapshot) -> Result<Snapshot> {
    if previous.record["id"] != current.record["id"]
        || previous.record["provider"] != current.record["provider"]
        || previous.record["identity"] != current.record["identity"]
        || previous.record["desktop"] != current.record["desktop"]
        || previous.credentials.binding() != current.credentials.binding()
    {
        return Err(account::changed());
    }
    if account::same(previous, &current) {
        return Err(Error::http(409, "Qoder 续期结果未保存，请重试"));
    }
    usable(&current.credentials)?;
    Ok(current)
}
fn upstream(previous: &Credentials) -> Result<Credentials> {
    if let Some(pat) = previous.pat() {
        return auth::exchange_pat(pat, Some(&previous.machine));
    }
    let refresh = previous.oauth_refresh();
    if refresh.is_empty() {
        return Err(Error::http(401, "Qoder 缺少刷新令牌"));
    }
    let path = if refresh.starts_with("jrt-") {
        endpoints::JOB_REFRESH
    } else {
        endpoints::DEVICE_REFRESH
    };
    let reply = client::request(
        "POST",
        endpoints::OPEN_API,
        path,
        client::refresh_headers(),
        Some(&json!({ "refresh_token": refresh })),
        "令牌续期",
    )?;
    if matches!(reply.status, 401 | 403) {
        return Err(Error::http(
            401,
            "Qoder 刷新令牌已失效，请重新登录国际版账号",
        ));
    }
    let data = client::accepted(reply, "令牌续期")?;
    // id 是设备会话号，不能当用户标识。
    let user = util::text(&data, &["user_id", "uid", "userId"]);
    let machine = util::text(&data, &["machine_id", "machineId"]);
    if !user.is_empty() && user != previous.user
        || !machine.is_empty() && machine != previous.machine
    {
        return Err(account::changed());
    }
    let access = util::secret(
        &data,
        &[
            "device_token",
            "deviceToken",
            "token",
            "access_token",
            "accessToken",
        ],
    )?;
    let new_refresh = util::secret(&data, &["refresh_token", "refreshToken"])?;
    // 两族均会轮换；缺另一半不能保存一组已经不能再次续期的凭据。
    if access.is_empty()
        || access.contains('|')
        || new_refresh.is_empty()
        || new_refresh.contains('|')
    {
        return Err(
            Error::http(502, "Qoder 续期未返回完整凭据，结果待确认；请重新授权")
                .with_code("qoder_refresh_unconfirmed"),
        );
    }
    let mut next = previous.clone();
    next.access = access;
    next.refresh = format!("{new_refresh}|{}|{}", previous.user, previous.machine);
    let fallback = if refresh.starts_with("jrt-") {
        endpoints::PAT_LIFETIME
    } else {
        endpoints::DEVICE_LIFETIME
    };
    next.expires = Some(util::expiry(&data, fallback)?);
    next.complete()?;
    Ok(next)
}
