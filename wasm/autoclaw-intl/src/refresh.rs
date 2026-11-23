//! 续期由宿主账号锁合并并发，并以账号 secret 原值 CAS 落地，不依赖静态全局。
//! 只在明确业务码 400002 时降级 agent-refresh，一次强刷不因 TTL 返回旧结果。
//! 本机引用只在独立授权后续期并写回固定登录文件，未授权时仍只读。
use super::{account, client, desktop, endpoints, util};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

const CONTROL: &str = "refresh-control";

pub fn expiring_within(secret: &Value, min_lead_ms: i64) -> bool {
    let margin = endpoints::REFRESH_MARGIN.max(min_lead_ms.max(0));
    account::expires(secret)
        .is_some_and(|at| at.saturating_sub(margin) <= sdk::now())
}
pub fn expiring(secret: &Value) -> bool {
    expiring_within(secret, 0)
}
pub fn due(min_lead_ms: i64) -> Result<bool> {
    let initial = account::read()?;
    if initial.local {
        let status = sdk::desktop_refresh::status(endpoints::PROVIDER)?;
        return Ok(status["authorized"] == true && status["refreshable"] == true && expiring_within(&initial.secret, min_lead_ms));
    }
    if account::refresh_token(&initial.secret).is_empty() {
        return Ok(false);
    }
    let _lock = AccountLock::acquire()?;
    let snapshot = account::read()?;
    if !account::same(&initial, &snapshot) {
        return Ok(false);
    }
    let before = util::transient(CONTROL)?;
    if !before.compatible {
        return Err(Error::http(
            409,
            "AutoClaw 国际版续期状态格式已变化，请重新发起",
        ));
    }
    if expiring_within(&snapshot.secret, min_lead_ms) {
        return Ok(true);
    }
    // 维护间隔只从成功续期起算，失败不阻挡下一轮调度。
    Ok(before.value["stamp"] != account::stamp(&snapshot)
        || sdk::now().saturating_sub(before.value["refreshedAt"].as_i64().unwrap_or(0))
            >= endpoints::HOURLY_REFRESH)
}

pub fn ensure() -> Result<String> {
    run(false)
}
pub fn force() -> Result<String> {
    run(true)
}

fn run(force: bool) -> Result<String> {
    let initial = account::read()?;
    if initial.local {
        if force || expiring(&initial.secret) {
            let status = sdk::desktop_refresh::status(endpoints::PROVIDER)?;
            if status["authorized"] == true && status["refreshable"] == true {
                sdk::desktop_refresh::refresh(endpoints::PROVIDER)?;
                return desktop::synchronize(&initial.record);
            }
            if account::expires(&initial.secret).is_some_and(|at| at <= sdk::now()) {
                return Err(Error::http(401, "AutoClaw 国际本机登录态已过期，请授权本机续期或在官方客户端重新登录"));
            }
            if force { return desktop::synchronize(&initial.record); }
        }
        return Ok(account::access(&initial.secret));
    }
    if !force && (!expiring(&initial.secret) || account::refresh_token(&initial.secret).is_empty())
    {
        return Ok(account::access(&initial.secret));
    }
    let _lock = AccountLock::acquire()?;
    let current = account::read()?;
    if !account::same(&initial, &current) {
        return current_token(&initial, &current);
    }
    if !force && !expiring(&current.secret) {
        return Ok(account::access(&current.secret));
    }
    let refresh = account::refresh_token(&current.secret);
    if refresh.is_empty() {
        return Err(Error::http(
            401,
            "AutoClaw 这份凭据没有刷新令牌，请重新登录或更新官方客户端登录态",
        ));
    }
    let before = util::transient(CONTROL)?;
    if !before.compatible {
        return Err(Error::http(
            409,
            "AutoClaw 国际版续期状态格式已变化，请重新发起",
        ));
    }
    // 并发只由当前调用持有的账号锁保护，不用历史失败标记拦截后续续期。
    let now = sdk::now();
    let next = upstream(&current.secret)?;
    let unchanged = account::access(&next) == account::access(&current.secret)
        && account::refresh_token(&next) == refresh;
    if unchanged && account::expires(&next).is_none_or(|expires| expires <= sdk::now()) {
        return Err(Error::http(
            409,
            "AutoClaw 续期仍返回已过期或无法确认有效期的旧凭据，请重新登录",
        ));
    }
    if account::expires(&next).is_some_and(|expires| expires <= sdk::now()) {
        return Err(Error::http(
            409,
            "AutoClaw 续期返回了已过期凭据，请重新登录",
        ));
    }
    let latest = account::read()?;
    if !account::same(&current, &latest) {
        return current_token(&current, &latest);
    }
    let updated = if unchanged {
        true
    } else if current.local {
        desktop::save(&current, &next)?
    } else {
        let mut stored = account::decode(&current.record["secret"])?;
        let fields = account::project(&next);
        // 与原生相同：续期只替换 token/设备/有效期，不补写 userId 改变存量去重身份。
        for key in [
            "token",
            "accessToken",
            "refreshToken",
            "deviceId",
            "expiresAt",
            "tokenExpiresAt",
        ] {
            if let Some(value) = fields.get(key) {
                stored[key] = value.clone();
                if let Some(auth) = stored["auth"].as_object_mut() {
                    auth.insert(key.into(), value.clone());
                }
            }
        }
        account::save(&current, stored)?
    };
    if !updated {
        let latest = account::read()?;
        if !account::same(&current, &latest) {
            return current_token(&current, &latest);
        }
        return Err(Error::http(
            409,
            "AutoClaw 续期结果未能保存，未将未保存的凭据当成成功",
        ));
    }
    let finished = account::read()?;
    let token = current_token(&current, &finished)?;
    util::put(
        CONTROL,
        &before,
        json!({ "stamp": account::stamp(&finished), "refreshedAt": now }),
        86_400_000,
    )?;
    Ok(token)
}

fn current_token(previous: &account::Snapshot, current: &account::Snapshot) -> Result<String> {
    let old_user = account::user(&previous.secret);
    let new_user = account::user(&current.secret);
    if previous.record["id"] != current.record["id"]
        || previous.record["provider"] != current.record["provider"]
        || previous.record["identity"] != current.record["identity"]
        || previous.record["desktop"] != current.record["desktop"]
        || previous.local != current.local
        || !old_user.is_empty() && old_user != new_user
    {
        return Err(Error::http(409, "AutoClaw 账号身份已变化，请重新发起请求")
            .with_code("identity_changed"));
    }
    Ok(account::access(&current.secret))
}

fn upstream(previous: &Value) -> Result<Value> {
    let mut body =
        json!({ "refresh_token": account::refresh_token(previous), "source_id": "autoclaw" });
    let device = account::device(previous);
    if !device.is_empty() {
        body["device_id"] = json!(device);
    }
    let token = account::access(previous);
    let mut payload = client::post("/userapi/v1/refresh", &token, &body, "续期")?;
    if util::integer(&payload["code"]) == Some(400_002) {
        payload = client::post("/userapi/v1/agent-refresh", &token, &body, "备用续期")?;
    }
    client::strict_business(&payload, "续期")?;
    let access = util::strip_bearer(&util::text(&payload["data"], &["access_token"]));
    util::token_valid(&access, false)
        .map_err(|_| Error::http(502, "AutoClaw 续期没有返回有效访问令牌"))?;
    let refresh = util::strip_bearer(&util::text(&payload["data"], &["refresh_token"]));
    util::token_valid(&refresh, true)?;
    if access.starts_with("enc:") || refresh.starts_with("enc:") {
        return Err(Error::http(
            502,
            "AutoClaw 续期返回了非预期凭据形态，未调用本机解密",
        ));
    }
    let mut next = previous.clone();
    next["token"] = json!(access);
    next["accessToken"] = json!(access);
    next["refreshToken"] = json!(if refresh.is_empty() {
        account::refresh_token(previous)
    } else {
        refresh
    });
    let user = util::scalar(&util::claims(&access)["user_id"]);
    if !user.is_empty() && !account::user(previous).is_empty() && user != account::user(previous) {
        return Err(
            Error::http(409, "AutoClaw 续期返回了不同账号身份，未覆盖原账号")
                .with_code("identity_changed"),
        );
    }
    if !user.is_empty() {
        next["userId"] = json!(user);
    }
    let expires = util::jwt_expiry(&access).or_else(|| util::integer(&payload["data"]["expires_in"])
        .filter(|seconds| *seconds > 0).and_then(|seconds| seconds.checked_mul(1000))
        .and_then(|duration| sdk::now().checked_add(duration)));
    next["expiresAt"] = json!(expires);
    next["tokenExpiresAt"] = json!(expires);
    Ok(next)
}
