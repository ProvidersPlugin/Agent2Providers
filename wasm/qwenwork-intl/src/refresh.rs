//! 国际设备令牌按原生千问基线提前一小时维护，携带本家client/redirect；兼容性未实测。
//! 续期结果未知按原secret持久保留，不复用一次性材料；仍有效访问可用，不虚构刷新成功。
//! 锁内比较原始 secret 后回写，保持同一账号和机器编号；没有作业令牌或 PAT 分支。
use super::{
    account::{self, Snapshot},
    client,
    credentials::Credentials,
    endpoints, util,
};
use agent2api_wasm_sdk::{json, AccountLock, Error, Result, Value};

const CONTROL: &str = "refresh-control";
pub fn due(min_lead_ms: i64) -> Result<bool> {
    let current = account::read()?;
    Ok(current.credentials.is_biz != Some(true) && current.credentials.expiring_within(min_lead_ms)?
        && agent2api_wasm_sdk::refresh_guard::available("credential-refresh", current.credentials.oauth_refresh())?)
}
pub fn ensure(force: bool) -> Result<Snapshot> {
    let initial = account::ready()?;
    if !force && !initial.credentials.expiring()? {
        let now = util::now()?;
        if initial.credentials.expires.is_some_and(|at| at <= now) {
            return Err(Error::http(
                401,
                "千问办公访问令牌已过期且无可用续期材料，请重新授权",
            ));
        }
        account::unchanged(&initial)?;
        return Ok(initial);
    }
    let _lock = AccountLock::acquire()?;
    let current = account::read()?;
    if !account::same(&initial, &current) {
        return compatible(&initial, current);
    }
    if !force && !current.credentials.expiring()? {
        account::unchanged(&current)?;
        let now = util::now()?;
        if current.credentials.expires.is_some_and(|at| at <= now) {
            return Err(Error::http(401, "千问办公访问凭据已过期，请重新授权"));
        }
        return Ok(current);
    }
    if !current.credentials.can_refresh() {
        return Err(Error::http(401, "千问办公没有刷新凭据，请重新授权"));
    }
    if current.credentials.is_biz == Some(true) {
        return Err(Error::http(409, "千问办公企业身份不能按个人设备凭据续期，请更新完整企业登录记录；未消费刷新令牌")
            .with_code("identity_changed"));
    }
    let now = util::now()?;
    let stamp = util::hash(
        &json!([
            current.credentials.binding(),
            current.credentials.oauth_refresh()
        ])
        .to_string(),
    );
    let before = util::transient(CONTROL)?;
    if !before.compatible || before.value["stamp"] == stamp && !before.value.is_null() {
        return usable_or_error(
            force,
            &current,
            Error::http(
                409,
                "千问办公上轮续期结果尚未确认，请重新授权；未重复使用旧刷新凭据",
            ),
        );
    }
    let marker = util::uuid()?;
    if !util::put(
        CONTROL,
        &before,
        json!({ "stamp": stamp, "marker": marker, "busyUntil": now + 120_000 }),
        300_000,
    )? {
        return Err(Error::http(409, "千问办公续期状态已变化"));
    }
    let reserved = util::transient(CONTROL)?;
    if reserved.value["marker"] != marker {
        return Err(Error::http(409, "千问办公续期预约已变化，未消耗刷新凭据"));
    }
    account::unchanged(&current)?;
    let reservation = agent2api_wasm_sdk::refresh_guard::Reservation::acquire(
        "credential-refresh", current.credentials.oauth_refresh())?;
    let next = match upstream(&current.credentials) {
        Ok(next) => next,
        Err(error) => {
            // 一次发送的失败不能证明旧材料未被消耗；状态按原secret持久保留，直到换入新资料。
            return usable_or_error(force, &current, error);
        }
    };
    let observed = util::transient(CONTROL)?;
    if observed.revision != reserved.revision || observed.value["marker"] != marker {
        return Err(Error::http(409, "千问办公续期来源状态已变化，未交付该结果"));
    }
    if next.binding() != current.credentials.binding() {
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
    if finished.credentials.binding() != next.binding()
        || finished.credentials.access != next.access || finished.credentials.refresh != next.refresh {
        return Err(Error::http(409, "千问办公续期保存结果未确认，未清除旧凭据预约"));
    }
    reservation.complete()?;
    if !util::put(CONTROL, &reserved, Value::Null, 1000)? {
        return Err(Error::http(409, "千问办公续期保存后的状态未确认，请刷新核对；不会重复换证"));
    }
    compatible(&current, finished)
}
fn usable_or_error(force: bool, current: &Snapshot, error: Error) -> Result<Snapshot> {
    account::unchanged(current)?;
    let now = util::now()?;
    if !force && !util::terminal(&error) && current.credentials.expires.is_some_and(|at| at > now) {
        // 提前维护失败与访问凭据过期不同；仍有效时可继续只读使用，不宣称续期成功。
        return Ok(current.clone());
    }
    Err(error)
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
        return Err(Error::http(409, "千问办公续期结果未保存，请重试"));
    }
    let now = util::now()?;
    if current.credentials.expires.is_some_and(|at| at <= now) {
        return Err(Error::http(401, "千问办公续期后凭据仍已过期，请重新授权"));
    }
    Ok(current)
}
fn upstream(previous: &Credentials) -> Result<Credentials> {
    let refresh = previous.oauth_refresh();
    if refresh.is_empty() {
        return Err(Error::http(401, "千问办公缺少刷新令牌"));
    }
    let reply = client::request(
        "POST",
        endpoints::DEVICE_REFRESH,
        client::refresh_headers()?,
        Some(
            &json!({ "refresh_token": refresh, "target": endpoints::REFRESH_TARGET,
            "client_id": endpoints::CLIENT_ID, "redirect_uri": endpoints::REDIRECT_URI }),
        ),
        "令牌续期",
    )?;
    if matches!(reply.status, 401 | 403) {
        return Err(Error::http(
            401,
            "千问办公刷新凭据已失效，请重新授权；设备令牌过期太久可能无法续期",
        ));
    }
    let payload = client::accepted(reply, "令牌续期")?;
    let data = client::unwrap_data(&payload);
    super::credentials::owner(data)?;
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
    next.refresh = format!("{new_refresh}|{}|{}", previous.user, previous.machine);
    next.expires = util::token_expiry(data, &next.access)?;
    // 没有到期证据时保持未知，不以30天兜底覆盖服务端真实有效时长。
    next.complete()?;
    Ok(next)
}
