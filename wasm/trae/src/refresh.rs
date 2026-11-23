//! 旧原生刷新链：24小时临期/15天签发龄，账号锁单飞，轮换成功先CAS落盘。
//! 仅400/10101归属拒绝才换已知ClientID；设备密钥和未知secret字段原样保留。
//! 网页与历史凭据先走 /cloudide/api/v3/trae/oauth/ExchangeToken，显式被拒且有原设备密钥时再退同设备 DeviceProof。
//! 不生成替代设备、不复用已发出但结果未知的refreshToken请求。

use super::{account, client, credentials, device, endpoints, errors, headers, util};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

pub fn client_ids(secret: &Value) -> Vec<String> {
    let primary = endpoints::client_id(credentials::variant(secret));
    let other = if primary == endpoints::SOLO_CLIENT {
        endpoints::IDE_CLIENT
    } else {
        endpoints::SOLO_CLIENT
    };
    let mut out = Vec::new();
    for id in [
        util::text(secret, &["authClientId"]),
        primary.into(),
        other.into(),
    ] {
        if !id.is_empty() && !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

pub fn token_fields(body: &Value) -> Result<Value> {
    for result in [body.get("Result"), body.get("result")]
        .into_iter()
        .flatten()
    {
        let token = util::header(
            &util::text(result, &["Token", "token", "AccessToken", "accessToken"]),
            16384,
        )?;
        if token.is_empty() {
            continue;
        }
        let mut fields = json!({ "accessToken": token });
        let refresh = util::header(
            &util::text(result, &["RefreshToken", "refreshToken", "refresh_token"]),
            16384,
        )?;
        if !refresh.is_empty() {
            fields["refreshToken"] = json!(refresh);
        }
        if let Some(at) = ["TokenExpireAt", "tokenExpireAt", "ExpiresAt", "expiresAt"]
            .iter()
            .find_map(|field| util::millis(&result[*field]))
            .or_else(|| {
                [
                    "TokenExpireDuration",
                    "tokenExpireDuration",
                    "ExpiresIn",
                    "expiresIn",
                ]
                .iter()
                .find_map(|field| util::integer(&result[*field]))
                .filter(|duration| *duration > 0)
                .and_then(|duration| {
                    let delta_ms = if duration > 1_000_000_000 {
                        duration
                    } else {
                        duration.saturating_mul(1000)
                    };
                    util::now().ok().map(|now| now.saturating_add(delta_ms))
                })
            })
        {
            fields["expiresAt"] = json!(at);
        }
        let id = util::header(
            &util::text(result, &["ClientID", "clientId", "client_id"]),
            256,
        )?;
        if !id.is_empty() {
            fields["authClientId"] = json!(id);
        }
        return Ok(fields);
    }
    Err(Error::http(
        502,
        "Trae 换证响应缺少有效访问令牌，未覆盖旧凭据",
    ))
}

enum ExchangeOutcome {
    Success(Value),
    Rejected(Error),
    Fatal(Error),
}

fn ensure_unchanged(secret: &Value) -> Result<()> {
    if credentials::parse(&account::current()?["secret"])? != *secret {
        return Err(
            Error::http(409, "Trae 续期前账号材料已变化").with_code("local_precondition"),
        );
    }
    Ok(())
}

fn post_exchange(
    secret: &Value,
    path: &str,
    headers: &Value,
    mut build_body: impl FnMut(&str) -> Result<Value>,
) -> Result<ExchangeOutcome> {
    let hosts = util::account_hosts(&util::text(secret, &["apiHost"]), false)?;
    let mut last: Option<Error> = None;
    for id in client_ids(secret) {
        let mut mismatch = false;
        for host in &hosts {
            if let Err(error) = ensure_unchanged(secret) {
                return Ok(ExchangeOutcome::Fatal(error));
            }
            let body = build_body(&id)?;
            let reply = match client::post(&format!("{host}{path}"), headers.clone(), &body) {
                Ok(reply) => reply,
                // 一次性续期发出后结果未知，不通过换host或换端点重复消费同一枚refreshToken。
                Err(error) => return Ok(ExchangeOutcome::Fatal(error)),
            };
            if (200..300).contains(&reply.status) && !errors::failed(&reply.body) {
                let mut fields = match token_fields(&reply.body) {
                    Ok(fields) => fields,
                    Err(error) => return Ok(ExchangeOutcome::Fatal(error)),
                };
                if fields["authClientId"].is_null() {
                    fields["authClientId"] = json!(id);
                }
                return Ok(ExchangeOutcome::Success(fields));
            }
            let attribution = reply.status == 400
                && (errors::code(&reply.body) == Some(10101)
                    || errors::message(&reply.body).contains("not matched to the client"));
            let err = errors::upstream(reply.status, &reply.body);
            if reply.status >= 500 {
                return Ok(ExchangeOutcome::Fatal(err));
            }
            last = Some(err.clone());
            if attribution {
                mismatch = true;
                break;
            }
            // 鉴权/设备/额度拒绝不是换域名的理由；只对明确路径不可达保留原生备用源。
            if !matches!(reply.status, 404 | 405) {
                return Ok(ExchangeOutcome::Rejected(err));
            }
        }
        if !mismatch {
            break;
        }
    }
    Ok(ExchangeOutcome::Rejected(last.unwrap_or_else(|| {
        Error::http(502, "Trae 续期未成功，已保留旧凭据")
    })))
}

fn legacy_exchange(secret: &Value, refresh: &str) -> Result<ExchangeOutcome> {
    let headers = headers::oauth(None);
    post_exchange(secret, endpoints::REFRESH, &headers, |id| {
        Ok(json!({
            "ClientID": id,
            "RefreshToken": refresh,
            "ClientSecret": "-",
            "UserID": ""
        }))
    })
}

fn bound_exchange(secret: &Value, refresh: &str) -> Result<Option<ExchangeOutcome>> {
    let public = util::text(secret, &["devicePublicKey"]);
    let private = util::text(secret, &["devicePrivateKey"]);
    let device_id = util::text(secret, &["deviceId"]);
    let machine = util::text(secret, &["machineId"]);
    if public.is_empty() || private.is_empty() || device_id.is_empty() || machine.is_empty() {
        return Ok(None);
    }
    if device::validate(&public, &private).is_err() {
        return Ok(None);
    }
    let os_info = headers::platform()?;
    let headers = headers::oauth(Some(""));
    let outcome = post_exchange(secret, endpoints::EXCHANGE, &headers, |id| {
        let platform = if id == endpoints::SOLO_CLIENT {
            "SOLO_PC"
        } else {
            "IDE_PC"
        };
        Ok(json!({
            "ClientID": id,
            "ClientSecret": "",
            "RefreshToken": refresh,
            "IDEVersion": endpoints::IDE_VERSION,
            "DeviceInfo": {
                "DeviceID": device_id,
                "MachineID": machine,
                "PlatformCode": platform,
                "DeviceType": "PC",
                "DeviceName": "",
                "DeviceModel": "",
                "ClientVersion": endpoints::IDE_VERSION,
                "DevicePublicKey": public,
                "DeviceBrand": "",
                "DeviceCPU": "",
                "OSInfo": os_info,
                "OSVersion": ""
            },
            "DeviceProof": device::proof(endpoints::EXCHANGE, id, refresh, &private)?
        }))
    })?;
    Ok(Some(outcome))
}

fn preferred_error(legacy: Error, bound: Error) -> Error {
    let legacy_auth = matches!(
        legacy.code.as_deref(),
        Some("trae_session_expired" | "trae_device_rejected")
    );
    let bound_auth = matches!(
        bound.code.as_deref(),
        Some("trae_session_expired" | "trae_device_rejected")
    );
    if legacy_auth && !bound_auth {
        return legacy;
    }
    if legacy.code.as_deref() == Some("trae_client_mismatch")
        && bound.code.as_deref() == Some("trae_upstream")
    {
        return legacy;
    }
    if matches!(bound.status, Some(404 | 405)) && !matches!(legacy.status, Some(404 | 405)) {
        return legacy;
    }
    bound
}

pub fn exchange(secret: &Value) -> Result<Value> {
    let refresh = util::text(secret, &["refreshToken"]);
    if refresh.is_empty() {
        return Err(
            Error::http(401, "Trae 账号没有续期凭据，请重新登录").with_code("trae_session_expired")
        );
    }
    let legacy_error = match legacy_exchange(secret, &refresh)? {
        ExchangeOutcome::Success(fields) => return Ok(fields),
        ExchangeOutcome::Fatal(error) => return Err(error),
        ExchangeOutcome::Rejected(error) => error,
    };
    match bound_exchange(secret, &refresh)? {
        Some(ExchangeOutcome::Success(fields)) => Ok(fields),
        Some(ExchangeOutcome::Fatal(error)) => Err(error),
        Some(ExchangeOutcome::Rejected(bound_error)) => {
            Err(preferred_error(legacy_error, bound_error))
        }
        None => Err(legacy_error),
    }
}

fn merge(secret: &Value, patch: &Value) -> Result<Value> {
    let mut next = secret.clone();
    let mut auth = &mut next;
    for key in ["pluginSecret", "secret", "auth"] {
        if auth.get(key).is_some_and(Value::is_object) {
            auth = &mut auth[key];
        }
    }
    let target = auth
        .as_object_mut()
        .ok_or_else(|| Error::http(409, "Trae 已保存凭据格式无效"))?;
    for (key, value) in patch.as_object().into_iter().flatten() {
        if !value.is_null() && value.as_str() != Some("") {
            target.insert(key.clone(), value.clone());
        }
    }
    if !patch["expiresAt"].is_number() {
        // 新token无到期读数时不能继续沿用旧token的时刻或其它旧别名。
        for key in ["expiresAt", "expiresAtMs", "TokenExpireAt", "tokenExpireAt"] {
            target.remove(key);
        }
    } else {
        for key in ["expiresAtMs", "TokenExpireAt", "tokenExpireAt"] {
            target.remove(key);
        }
    }
    // 已更新的auth可能仍包在旧平铺时间之外；父级旧别名不能重新兜回新令牌。
    let mut outer = &mut next;
    for key in ["pluginSecret", "secret", "auth"] {
        if outer.get(key).is_some_and(Value::is_object) {
            for field in ["expiresAt", "expiresAtMs", "TokenExpireAt", "tokenExpireAt"] {
                if let Some(fields) = outer.as_object_mut() {
                    fields.remove(field);
                }
            }
            outer = &mut outer[key];
        }
    }
    Ok(next)
}

/// 调用方已经拿到账号锁；签到内部使用，避免同一次调用嵌套锁。
pub fn locked(record: Value, force: bool) -> Result<Value> {
    let secret = credentials::parse(&record["secret"])?;
    let can = !util::text(&secret, &["refreshToken"]).is_empty();
    if !force && (!can || !credentials::needs_refresh(&secret, util::now()?)) {
        credentials::token(&secret)?;
        return Ok(record);
    }
    let fields = exchange(&secret)?;
    let next = merge(&record["secret"], &fields)?;
    let normalized = credentials::parse(&next)?;
    let updated = sdk::host(
        "account.update",
        json!({ "expect": record["secret"], "secret": next,
        "public": account::view(&normalized) }),
    )?;
    if updated["updated"] != true {
        return Err(Error::http(
            409,
            "Trae 续期期间凭据被替换，新令牌未覆盖账号；请确认登录状态",
        )
        .with_code("credential_conflict"));
    }
    let mut record = record;
    record["secret"] = next;
    Ok(record)
}

pub fn ensure(force: bool, observed: Option<&Value>) -> Result<Value> {
    let before = account::current()?;
    let expected = observed.unwrap_or(&before["secret"]);
    let _guard = AccountLock::acquire()?;
    let latest = account::current()?;
    if latest["secret"] != *expected {
        // 并发任务已经刷新，使用落盘的新值，不再把新refreshToken轮换一次。
        let normalized = credentials::parse(&latest["secret"])?;
        if force {
            credentials::token(&normalized)?;
            return Ok(latest);
        }
        return locked(latest, false);
    }
    locked(latest, force)
}

pub fn expiring(min_lead_ms: i64) -> Result<bool> {
    let record = account::current()?;
    let secret = credentials::parse(&record["secret"])?;
    Ok(!util::text(&secret, &["refreshToken"]).is_empty()
        && credentials::needs_refresh_within(&secret, util::now()?, min_lead_ms))
}
