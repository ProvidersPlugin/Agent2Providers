//! 凭证单飞由宿主账号锁实现；锁内回读、比较旧 secret 再写新值，不靠 WASM 全局。
//! 同 token 仅延长有效期也是续期成功；省略 refreshToken/sgcookie 时保留旧值。

use crate::{
    account,
    credentials::{self, Credentials},
    endpoints, network, util,
};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    AccountLock, Error, Result, Value,
};

pub struct Session {
    pub secret: Value,
    pub credentials: Credentials,
}

fn read() -> Result<Session> {
    let record = sdk::account()?;
    if record["provider"].as_str() != Some(endpoints::ID)
        || sdk::account_disabled(&record)
    {
        return Err(Error::http(409, "Accio 当前账号已停用或归属变化"));
    }
    let secret = record["secret"].clone();
    let credentials = Credentials::parse(&secret)?;
    Ok(Session {
        secret,
        credentials,
    })
}

pub fn check(session: &Session) -> Result<()> {
    let current = read()?;
    if current.secret != session.secret {
        return Err(
            Error::http(409, "Accio 账号在操作期间已变化，未继续使用旧凭据")
                .with_code("accio_account_changed"),
        );
    }
    Ok(())
}

fn same_account(current: &Credentials, previous: &Credentials) -> bool {
    current.device == previous.device && (previous.user.is_empty() || current.user == previous.user)
}

fn save(before: &Value, after: &Value) -> Result<bool> {
    let value = sdk::host(
        "account.update",
        json!({ "expect": before, "secret": after, "public": account::view(after)? }),
    )?;
    Ok(value["updated"] == true)
}

fn merge(before: &Value, credential: &Credentials) -> Value {
    let mut next = before.clone();
    for (key, value) in credential.value().as_object().into_iter().flatten() {
        if !value.is_null() && !value.as_str().is_some_and(str::is_empty) {
            next[key] = value.clone();
        }
    }
    // 兼容旧记录中的同义键，避免导出后读到互相矛盾的令牌。
    for key in ["access_token", "token"] {
        if next.get(key).is_some() {
            next[key] = json!(credential.access);
        }
    }
    if next.get("refresh_token").is_some() {
        next["refresh_token"] = json!(credential.refresh);
    }
    for key in ["expires_at", "expires"] {
        if next.get(key).is_some() {
            next[key] = json!(credential.expires);
        }
    }
    next
}

/// 调用者已持有账号锁；设备号先落盘，再允许任何转发使用它。
fn load_locked() -> Result<Session> {
    let mut session = read()?;
    let before = session.credentials.device.clone();
    session.credentials.ensure_device(false)?;
    if session.credentials.device == before {
        return Ok(session);
    }
    let next = merge(&session.secret, &session.credentials);
    if save(&session.secret, &next)? {
        session.secret = next;
        return Ok(session);
    }
    let latest = read()?;
    if !credentials::header_safe(&latest.credentials.device)
        || (!session.credentials.user.is_empty()
            && latest.credentials.user != session.credentials.user)
        || latest.credentials.access != session.credentials.access
        || latest.credentials.refresh != session.credentials.refresh
    {
        return Err(Error::http(409, "Accio 账号已被其他操作修改，请重试"));
    }
    Ok(latest)
}

fn renew_locked(session: Session) -> Result<Session> {
    let current = &session.credentials;
    if current.refresh.is_empty() {
        return Err(Error::http(
            401,
            "Accio 账号缺少刷新凭据，请重新登录或导入完整凭证",
        ));
    }
    check(&session)?;
    let renewal = state::get(Scope::Account, "accio.renewal", 1)?;
    let material = util::fingerprint(&current.refresh);
    if !renewal.compatible || renewal.value["material"] == material {
        return Err(Error::http(
            409,
            "Accio 上次续期结果尚未确认，不能重复使用同一刷新凭据，请在官方客户端重新登录",
        )
        .with_code("accio_refresh_unconfirmed"));
    }
    if !state::compare_exchange(
        Scope::Account,
        "accio.renewal",
        1,
        renewal.revision,
        json!({ "material": material, "startedAt": util::now()? }),
        0,
    )? {
        return Err(Error::http(409, "Accio 续期状态已变化，请重试"));
    }
    // 轮换材料可能已被上游消耗；超时或未知结果不重新使用同一刷新令牌。
    let body = json!({ "accessToken": current.access, "refreshToken": current.refresh });
    let data = network::data(network::post(
        endpoints::REFRESH,
        &body,
        &current.secrets(),
    )?);
    check(&session)?;
    credentials::international(&data)?;
    let access = credentials::token(&data, &["accessToken", "access_token", "token"])?;
    if access.is_empty() {
        return Err(Error::http(502, "Accio 续期响应缺少访问令牌，旧凭据未覆盖"));
    }
    let mut fresh = current.clone();
    fresh.access = access;
    let refresh = credentials::token(&data, &["refreshToken", "refresh_token"])?;
    if !refresh.is_empty() {
        fresh.refresh = refresh;
    }
    let cookie = credentials::sgcookie(&data);
    if !cookie.is_empty() {
        fresh.sgcookie = cookie;
    }
    let user = util::text(&data, &["userId", "user_id", "uid"]);
    if !user.is_empty() && !current.user.is_empty() && user != current.user {
        return Err(Error::http(
            502,
            "Accio 续期返回了不同的账号身份，未覆盖旧凭据",
        ));
    }
    let ttl = util::now()?.saturating_add(3_600_000);
    fresh.expires = Some(
        ["expiresAt", "expires_at", "expires"]
            .iter()
            .find_map(|key| util::timestamp(data.get(*key)))
            .unwrap_or_else(|| {
                if endpoints::ID == "accio-cn" {
                    // 只拿到新令牌而省略到期时，不能把仍更晚的原有效期改短。
                    current.expires.unwrap_or(ttl).max(ttl)
                } else {
                    ttl
                }
            }),
    );
    if !fresh.advanced_from(current) {
        return Err(Error::http(502, "Accio 续期响应未推进有效凭据，未覆盖旧值"));
    }
    // 不用续期响应更改身份/设备。旧 token 摘要兜底身份的存储主键由宿主保持。
    let next = merge(&session.secret, &fresh);
    if save(&session.secret, &next)? {
        let completed = state::get(Scope::Account, "accio.renewal", 1)?;
        if !completed.compatible
            || completed.value["material"] != material
            || !state::compare_exchange(
                Scope::Account,
                "accio.renewal",
                1,
                completed.revision,
                Value::Null,
                0,
            )?
        {
            return Err(Error::http(
                409,
                "Accio 新凭据已保存，但续期状态变化，请重新读取账号",
            ));
        }
        return Ok(Session {
            secret: next,
            credentials: fresh,
        });
    }
    // 比较失败只能回读实际落库值，未落库的新令牌绝不拿去发请求。
    let latest = read()?;
    if !credentials::header_safe(&latest.credentials.device)
        || !same_account(&latest.credentials, current)
        || !latest.credentials.advanced_from(current)
    {
        return Err(Error::http(
            409,
            "Accio 续期期间账号已变化，新凭据未保存，请重新导入或重试",
        ));
    }
    Ok(latest)
}

pub fn ready() -> Result<Session> {
    let _lock = AccountLock::acquire()?;
    let current = load_locked()?;
    if current.credentials.expiring(util::now()?) {
        renew_locked(current)
    } else {
        Ok(current)
    }
}

pub fn snapshot() -> Result<Session> {
    let _lock = AccountLock::acquire()?;
    load_locked()
}

pub fn force() -> Result<Session> {
    let before = snapshot()?.credentials;
    after_rejection(&before)
}

pub fn after_rejection(rejected: &Credentials) -> Result<Session> {
    let _lock = AccountLock::acquire()?;
    let current = load_locked()?;
    if !same_account(&current.credentials, rejected) {
        return Err(
            Error::http(409, "Accio 重试期间账号身份或设备已变化，未续发旧请求")
                .with_code("accio_account_changed"),
        );
    }
    if current.credentials.advanced_from(rejected) {
        Ok(current)
    } else {
        renew_locked(current)
    }
}
