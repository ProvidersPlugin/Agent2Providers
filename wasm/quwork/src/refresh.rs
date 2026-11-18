//! 账号续期不等于模型续期；模型令牌可独立继续使用。只在显式 desktop 引用读取辅助。
//! 普通账号 CAS 写入；桌面覆盖只放宿主命名 conversation 内存状态，2小时自动失效。

use super::{account, client, desktop, endpoints, errors};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

const OVERLAY: &str = "desktop-credential-overlay";
const OVERLAY_KEY: &str = "credentials";
const OVERLAY_TTL: i64 = 7_200_000;

struct Snapshot {
    record: Value,
    secret: Value,
    stamp: String,
}

fn invalidate_overlay(revision: u64) -> Result<()> {
    sdk::state::conversation_compare_exchange(
        OVERLAY,
        OVERLAY_KEY,
        1,
        revision,
        Value::Null,
        1000,
    )?;
    Ok(())
}

fn read() -> Result<Snapshot> {
    let record = sdk::account()?;
    if !account::local(&record["secret"]) {
        let secret = account::parse(&record["secret"])?;
        account::unchanged(&record)?;
        return Ok(Snapshot {
            record,
            secret,
            stamp: String::new(),
        });
    }
    // 先取覆盖版本再读来源，来源变化时的CAS不能清掉另一轮刚写入的新覆盖。
    let cached = sdk::state::conversation_get(OVERLAY, OVERLAY_KEY, 1)?;
    let disk = match desktop::read(Some(&record["secret"])) {
        Ok(secret) => secret,
        Err(error) => {
            if !cached.value.is_null() {
                let _ = invalidate_overlay(cached.revision);
            }
            return Err(error);
        }
    };
    let stamp = desktop::stamp(&disk);
    if !cached.compatible {
        return Err(Error::http(
            409,
            "QuWork 桌面临时凭据版本不兼容，请重新导入",
        ));
    }
    let secret = if cached.value["stamp"] == stamp
        && cached.value["secret"].is_object()
        && cached.value["until"]
            .as_i64()
            .is_some_and(|until| until > sdk::now())
    {
        // provider/账号/身份均由宿主隔离，来源文件改变后本覆盖立即失效。
        let value = account::parse(&cached.value["secret"])?;
        if account::identity(&value) != account::identity(&record["secret"]) {
            return Err(Error::http(409, "QuWork 桌面凭据身份不一致，请重新导入")
                .with_code("identity_changed"));
        }
        value
    } else {
        if !cached.value.is_null() {
            invalidate_overlay(cached.revision)?;
        }
        disk
    };
    account::unchanged(&record)?;
    Ok(Snapshot {
        record,
        secret,
        stamp,
    })
}

pub fn snapshot() -> Result<Value> {
    Ok(read()?.secret)
}

pub fn expiring(secret: &Value) -> bool {
    !account::text(secret, &["accessToken"]).is_empty()
        && client::timestamp(&secret["expiresAt"])
            .is_some_and(|expiry| expiry <= sdk::now().saturating_add(endpoints::REFRESH_WINDOW))
}

pub fn refresh(force: bool) -> Result<Value> {
    let before = read()?;
    if !force && !expiring(&before.secret) {
        return Ok(before.secret);
    }
    let _lock = AccountLock::acquire()?;
    refresh_locked(before, force)
}

fn refresh_locked(before: Snapshot, force: bool) -> Result<Value> {
    let current = read()?;
    if account::identity(&current.secret) != account::identity(&before.secret) {
        return Err(Error::http(409, "QuWork 续期前账号身份已变化，请重新发起")
            .with_code("identity_changed"));
    }
    if current.record["secret"] != before.record["secret"]
        || current.secret != before.secret
        || !force && !expiring(&current.secret)
    {
        return Ok(current.secret);
    }
    let access = account::text(&current.secret, &["accessToken"]);
    if access.is_empty() {
        return Err(errors::missing_account());
    }
    let recent_key = format!(
        "refresh:{}",
        account::fingerprint(&account::text(&current.record, &["id"]))
    );
    let recent = sdk::session_get(&recent_key)?;
    if recent["stamp"] == desktop::stamp(&current.secret) {
        return Ok(current.secret);
    }
    account::unchanged(&current.record)?;
    let reply = client::request("GET", endpoints::REFRESH, &access, None)?;
    let data = reply.data("登录续期")?;
    let next = account::token(&account::text(&data, &["token"]))?;
    if next.is_empty() {
        return Err(Error::http(502, "QuWork 续期未返回登录凭证"));
    }
    let expiry = client::timestamp(&data["expire"])
        .or_else(|| account::jwt_expiry(&next))
        .unwrap_or_else(|| sdk::now().saturating_add(7_200_000));
    if next == access && expiry <= client::timestamp(&current.secret["expiresAt"]).unwrap_or(0) {
        return Err(Error::http(
            502,
            "QuWork 续期未更新凭证或有效期，请重新登录",
        ));
    }
    let mut secret = current.secret.clone();
    secret["accessToken"] = json!(next);
    secret["expiresAt"] = json!(expiry);
    secret["updatedAt"] = json!(sdk::now());
    let llm = account::token(&account::text(&data, &["llm_token"]))?;
    if !llm.is_empty() {
        secret["llmToken"] = json!(llm);
    }
    if account::identity(&secret) != account::identity(&current.secret) {
        return Err(
            Error::http(409, "QuWork 续期无法保持原账号身份，请重新导入完整登录态")
                .with_code("identity_changed"),
        );
    }
    let local = account::local(&current.record["secret"]);
    let overlay_before = if local {
        Some(sdk::state::conversation_get(OVERLAY, OVERLAY_KEY, 1)?)
    } else {
        None
    };
    let saved = if local {
        let disk = desktop::read(Some(&current.record["secret"]))?;
        if desktop::stamp(&disk) != current.stamp {
            return Ok(disk);
        }
        let mut reference = current.record["secret"].clone();
        reference["expiresAt"] = json!(expiry);
        reference["updatedAt"] = json!(sdk::now());
        reference
    } else {
        secret.clone()
    };
    let updated = sdk::host(
        "account.update",
        json!({ "expect": current.record["secret"],
        "secret": saved, "public": account::view(&saved) }),
    )?;
    if updated["updated"] != true {
        return snapshot();
    }
    if let Some(overlay) = overlay_before {
        if !overlay.compatible
            || !sdk::state::conversation_compare_exchange(
                OVERLAY,
                OVERLAY_KEY,
                1,
                overlay.revision,
                json!({ "stamp": current.stamp, "secret": secret,
                "until": sdk::now().saturating_add(OVERLAY_TTL) }),
                OVERLAY_TTL,
            )?
        {
            return Err(Error::http(409, "QuWork 桌面凭据覆盖已变化，请重试"));
        }
    }
    let _ = sdk::session_put(
        &recent_key,
        json!({ "stamp": desktop::stamp(&secret) }),
        5000,
    );
    Ok(secret)
}

pub fn model_token() -> Result<String> {
    let record = sdk::account()?;
    let is_local = account::local(&record["secret"]);
    let original = snapshot()?;
    let secret = if expiring(&original) {
        // 账号服务出故障时不误伤仍然有效的独立模型凭证。
        match refresh(false) {
            Ok(secret) => secret,
            Err(error) if error.code.as_deref() == Some("identity_changed") => return Err(error),
            Err(_) if is_local => snapshot()?,
            Err(_) => {
                account::unchanged(&record)?;
                original
            }
        }
    } else {
        original
    };
    let token = account::token(&account::text(&secret, &["llmToken"]))?;
    if token.is_empty() {
        return Err(
            Error::http(401, "QuWork 缺少模型凭证，请重新导入").with_code("quwork_model_expired")
        );
    }
    Ok(token)
}

pub fn ensure_account() -> Result<Value> {
    let secret = snapshot()?;
    let secret = if expiring(&secret) {
        refresh(false)?
    } else {
        secret
    };
    if account::text(&secret, &["accessToken"]).is_empty() {
        return Err(errors::missing_account());
    }
    Ok(secret)
}

pub fn protected(method: &str, path: &str, body: Option<&Value>) -> Result<client::Reply> {
    let secret = ensure_account()?;
    let record = sdk::account()?;
    let latest = snapshot()?;
    account::unchanged(&record)?;
    if account::parse(&latest)? != account::parse(&secret)? {
        return Err(
            Error::http(409, "QuWork 登录态在请求准备期间已变化，请重新发起")
                .with_code("identity_changed"),
        );
    }
    let reply = client::request(
        method,
        path,
        &account::text(&secret, &["accessToken"]),
        body,
    )?;
    account::unchanged(&record)?;
    if !reply.unauthorized() {
        return Ok(reply);
    }
    let fresh = refresh(true)?;
    let current = sdk::account()?;
    if account::parse(&snapshot()?)? != account::parse(&fresh)? {
        return Err(
            Error::http(409, "QuWork 续期后的账号已变化，请重新发起").with_code("identity_changed")
        );
    }
    let reply = client::request(method, path, &account::text(&fresh, &["accessToken"]), body)?;
    account::unchanged(&current)?;
    Ok(reply)
}
