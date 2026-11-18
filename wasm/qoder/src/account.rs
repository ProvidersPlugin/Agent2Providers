//! 国内身份固定，稳定去重键仍为 qoder-<uid>；令牌和 PAT 只留 secret。
//! 本机导入与设备编号读取是不同权限；账号不隐式读取官方登录文件。
use super::{
    auth,
    credentials::{self, Credentials},
    endpoints, util,
};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

#[derive(Clone)]
pub struct Snapshot {
    pub record: Value,
    pub credentials: Credentials,
}
pub fn read() -> Result<Snapshot> {
    let record = sdk::account()?;
    credentials::owner(&record)?;
    if sdk::account_disabled(&record) {
        return Err(changed());
    }
    let credentials = Credentials::parse(&record["secret"])?;
    if credentials.user.is_empty() {
        return Err(Error::http(
            400,
            "旧 Qoder 账号缺少用户标识，请重新导入完整记录",
        ));
    }
    Ok(Snapshot {
        record,
        credentials,
    })
}
pub fn ready() -> Result<Snapshot> {
    let before = read()?;
    if !before.credentials.machine.is_empty() {
        return Ok(before);
    }
    // 旧账号缺机器码时只生成一次，并先 CAS 持久化，不能每次签名随机换设备。
    let _lock = AccountLock::acquire()?;
    let current = read()?;
    if !same(&before, &current) {
        return Err(changed());
    }
    if !current.credentials.machine.is_empty() {
        return Ok(current);
    }
    let mut next = current.credentials.clone();
    next.complete()?;
    if !save(&current, &next)? {
        return Err(changed());
    }
    read()
}
pub fn same(left: &Snapshot, right: &Snapshot) -> bool {
    left.record["id"] == right.record["id"]
        && left.record["provider"] == right.record["provider"]
        && left.record["identity"] == right.record["identity"]
        && left.record["desktop"] == right.record["desktop"]
        && left.record["secret"] == right.record["secret"]
}
pub fn changed() -> Error {
    Error::http(409, "Qoder 账号已删除、重新导入或身份变化，请重试").with_code("identity_changed")
}
pub fn unchanged(snapshot: &Snapshot) -> Result<()> {
    if same(snapshot, &read()?) {
        Ok(())
    } else {
        Err(changed())
    }
}
pub fn identity(value: &Value) -> Result<String> {
    let credentials = Credentials::parse(value)?;
    Ok(if credentials.user.is_empty() {
        String::new()
    } else {
        format!("qoder-{}", credentials.user)
    })
}
pub fn view(value: &Value) -> Result<Value> {
    let value = credentials::decode(value)?;
    let mut credentials = Credentials::parse(&value)?;
    let name = util::text(&value, &["name"]);
    credentials.name = if name.is_empty() {
        if credentials.name.is_empty() {
            if credentials.user.is_empty() {
                endpoints::LABEL.into()
            } else {
                format!("Qoder {}", credentials.user)
            }
        } else {
            credentials.name.clone()
        }
    } else {
        name
    };
    credentials.validate_public()?;
    let tail = if credentials.access.chars().count() > 4 {
        credentials
            .access
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>()
    } else {
        String::new()
    };
    let source = util::text(&value, &["source"]);
    let source = if matches!(source.as_str(), "web" | "manual") {
        source
    } else {
        String::new()
    };
    let mut public_guard = credentials.clone();
    public_guard.name = source.clone();
    public_guard.validate_public()?;
    let mut view = json!({ "name": credentials.name, "uid": credentials.user, "userId": credentials.user,
        "tokenTail": tail, "edition": endpoints::PROVIDER, "editionLabel": "中国版",
        "source": source, "hasSession": true, "hasAccessToken": true,
        "hasRefreshToken": credentials.can_refresh(), "canRefresh": credentials.can_refresh(), "desktop": false });
    if !credentials.email.is_empty() {
        view["email"] = json!(credentials.email);
    }
    if let Some(at) = credentials.expires {
        view["expiresAt"] = json!(at);
    }
    Ok(view)
}
pub fn normalized(mut credentials: Credentials, source: &str, name: &str) -> Result<Value> {
    credentials.complete()?;
    let mut secret = credentials.value();
    secret["source"] = json!(source);
    let name = if name.trim().is_empty() {
        if credentials.name.is_empty() {
            format!("Qoder {}", credentials.user)
        } else {
            credentials.name.clone()
        }
    } else {
        name.trim().chars().take(64).collect()
    };
    secret["name"] = json!(name);
    Ok(
        json!({ "identity": identity(&secret)?, "desktop": false, "public": view(&secret)?, "secret": secret }),
    )
}
pub fn normalize(params: &Value) -> Result<Value> {
    let source = &params["source"];
    let value = match source["kind"].as_str() {
        Some("manual") => {
            let payload = &source["payload"];
            if payload.get("methodIndex").is_some()
                && !matches!(payload["methodIndex"].as_u64(), Some(1 | 2 | 3))
            {
                return Err(Error::http(400, "设备授权请从网页登录入口发起"));
            }
            let value = if payload["inputs"].is_object() {
                &payload["inputs"]
            } else {
                payload
            };
            if let Some(raw) = value["credentialJson"]
                .as_str()
                .filter(|value| !value.is_empty())
            {
                credentials::decode(&json!(raw))?
            } else {
                credentials::decode(value)?
            }
        }
        Some("file_text") => credentials::decode(&source["content"])?,
        Some("desktop") => {
            return Err(Error::http(
                400,
                "当前主程序尚未接通 Qoder 中国版本机账号导入，请使用设备授权或手动凭据",
            ))
        }
        _ => {
            return Err(Error::http(
                400,
                "Qoder 请使用设备授权、PAT、手动凭据或 JSON 文本导入",
            ))
        }
    };
    let credentials = auth::prepare(&value)?;
    normalized(credentials, "manual", &util::text(params, &["name"]))
}
pub fn save(current: &Snapshot, fresh: &Credentials) -> Result<bool> {
    fresh.validate_public()?;
    if fresh.user != current.credentials.user
        || !current.credentials.machine.is_empty() && fresh.machine != current.credentials.machine
    {
        return Err(changed());
    }
    unchanged(current)?;
    let mut next = credentials::decode(&current.record["secret"])?;
    for (key, value) in fresh.value().as_object().into_iter().flatten() {
        if value.is_null() || value == "" {
            continue;
        }
        next[key] = value.clone();
        if let Some(auth) = next["auth"].as_object_mut() {
            auth.insert(key.clone(), value.clone());
        }
    }
    for (canonical, aliases) in [
        ("accessToken", &["access", "token", "access_token", "device_token", "deviceToken"][..]),
        ("refreshToken", &["refresh", "refresh_token"][..]),
        ("expiresAt", &["expires", "expires_at", "tokenExpiresAt"][..]),
        ("userId", &["user_id", "uid"][..]),
        ("machineId", &["machine_id", "loginDeviceId"][..]),
    ] {
        for alias in aliases {
            if next.get(*alias).is_some() { next[*alias] = next[canonical].clone(); }
            let value = next[canonical].clone();
            if let Some(auth) = next["auth"].as_object_mut() {
                if auth.contains_key(*alias) { auth.insert((*alias).into(), value); }
            }
        }
    }
    if let Some(name) = current.record["public"]["name"].as_str() {
        next["name"] = json!(name);
    }
    unchanged(current)?;
    let public = view(&next)?;
    Ok(sdk::host(
        "account.update",
        json!({ "expect": current.record["secret"], "secret": next, "public": public }),
    )?["updated"]
        == true)
}
