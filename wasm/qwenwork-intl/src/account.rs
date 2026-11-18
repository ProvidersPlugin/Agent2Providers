//! qwenwork-<uid> 旧业务身份由宿主 accounts.identity.v1 在 provider 内匹配，保留旧主键。
//! 账号读写仅限宿主当前上下文。缺机器编号先锁内一次性补齐并 CAS 保存，不能每次请求重建。
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
        return Err(Error::coded("千问办公国际账号已停用", "forbidden"));
    }
    let credentials = Credentials::parse(&record["secret"])?;
    if credentials.user.is_empty() {
        return Err(Error::http(
            400,
            "千问办公旧记录缺少用户身份，请重新导入完整记录",
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
    if !same(&current, &read()?) {
        return Err(changed());
    }
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
    Error::http(409, "千问办公账号已删除、重新导入或身份变化，请重试").with_code("identity_changed")
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
    if credentials.user.is_empty() {
        return Err(Error::http(400, "千问办公缺少稳定用户身份"));
    }
    if credentials.is_biz==Some(true) {
        let pool=util::hash(&json!([credentials.organization,credentials.team,credentials.realm]).to_string());
        return Ok(format!("qwenwork-{}-biz-{}",credentials.user,&pool[..24]));
    }
    Ok(format!("qwenwork-{}", credentials.user))
}
fn default_name(credentials: &Credentials) -> String {
    if !credentials.name.is_empty() {
        credentials.name.clone()
    } else if !credentials.email.is_empty() {
        credentials.email.chars().take(64).collect()
    } else if !credentials.user.is_empty() {
        format!("{} {}", endpoints::LABEL, credentials.user)
    } else {
        endpoints::LABEL.into()
    }
}
pub fn view(value: &Value) -> Result<Value> {
    let value = credentials::decode(value)?;
    let mut credentials = Credentials::parse(&value)?;
    let name = util::text(&value, &["name"]);
    credentials.name = if name.is_empty() {
        default_name(&credentials)
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
        "tokenTail": tail, "edition": endpoints::PROVIDER, "editionLabel": "国际版",
        "source": source, "hasSession": true, "hasAccessToken": true,
        "hasRefreshToken": credentials.can_refresh(), "canRefresh": credentials.can_refresh() && credentials.is_biz != Some(true), "desktop": false,
        "isBiz":credentials.is_biz,"organizationId":credentials.organization,"teamId":credentials.team });
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
        default_name(&credentials)
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
            credentials::owner(payload)?;
            if payload.get("methodIndex").is_some()
                && !matches!(payload["methodIndex"].as_u64(), Some(1 | 2))
            {
                return Err(Error::http(400, "设备授权请从网页登录入口发起"));
            }
            let value = if payload["inputs"].is_object() {
                &payload["inputs"]
            } else {
                payload
            };
            credentials::owner(value)?;
            if payload["methodIndex"] == 2
                && value["credentialJson"].as_str().is_none_or(str::is_empty)
                && payload["credentialJson"].as_str().is_none_or(str::is_empty)
            {
                return Err(Error::http(400, "请粘贴完整千问办公国际凭据 JSON"));
            }
            if let Some(raw) = value["credentialJson"]
                .as_str()
                .filter(|value| !value.is_empty())
            {
                credentials::decode(&json!(raw))?
            } else if let Some(raw) = payload["credentialJson"]
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
                "千问办公国际版不读取本机凭据，请使用设备授权或凭据文本",
            ))
        }
        _ => {
            return Err(Error::http(
                400,
                "千问办公请使用设备授权、手工凭据或 JSON 文本",
            ))
        }
    };
    normalized(
        auth::prepare(&value)?,
        "manual",
        &util::text(params, &["name"]),
    )
}
pub fn save(current: &Snapshot, fresh: &Credentials) -> Result<bool> {
    fresh.validate_public()?;
    if fresh.user != current.credentials.user
        || !current.credentials.machine.is_empty() && fresh.machine != current.credentials.machine
        || fresh.is_biz != current.credentials.is_biz || fresh.organization != current.credentials.organization
        || fresh.team != current.credentials.team || fresh.realm != current.credentials.realm
    {
        return Err(changed());
    }
    let mut next = credentials::decode(&current.record["secret"])?;
    let fresh_value = fresh.value();
    for (key, value) in fresh_value.as_object().into_iter().flatten() {
        if value.is_null() || value == "" {
            if key != "expiresAt" {
                continue;
            }
        }
        next[key] = value.clone();
        if let Some(auth) = next["auth"].as_object_mut() {
            auth.insert(key.clone(), value.clone());
        }
    }
    // 同一凭据的旧别名一并回写，不能把新 accessToken 与旧 access/device_token 同时留在记录里。
    for (canonical, aliases) in [
        (
            "accessToken",
            &[
                "access",
                "token",
                "access_token",
                "device_token",
                "deviceToken",
            ][..],
        ),
        ("refreshToken", &["refresh", "refresh_token"][..]),
        (
            "expiresAt",
            &["expires", "expires_at", "tokenExpiresAt"][..],
        ),
        ("userId", &["user_id", "uid"][..]),
        ("machineId", &["machine_id", "loginDeviceId"][..]),
    ] {
        if fresh_value[canonical].is_null() || fresh_value[canonical] == "" {
            if canonical != "expiresAt" {
                continue;
            }
        }
        for alias in aliases {
            if next.get(*alias).is_some() {
                next[*alias] = fresh_value[canonical].clone();
            }
            if let Some(auth) = next["auth"].as_object_mut() {
                if auth.contains_key(*alias) {
                    auth.insert((*alias).to_string(), fresh_value[canonical].clone());
                }
            }
        }
    }
    if next.get("user").is_some_and(Value::is_object) {
        next["user"]["id"] = json!(fresh.user);
        if !fresh.name.is_empty() {
            next["user"]["name"] = json!(fresh.name);
        }
        if !fresh.email.is_empty() {
            next["user"]["email"] = json!(fresh.email);
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
