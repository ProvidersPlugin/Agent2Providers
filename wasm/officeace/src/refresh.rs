//! 一次性刷新：账号锁内回读、原始 secret CAS；30分钟短期恢复状态放命名 conversation。
//! journal 只含轮换后控制面字段，不复制 P-256 私钥；私钥始终留在当前账号 secret。
//! 网络结果不明或实例中途结束时不重放旧 refresh token，模型 Basic 不受影响。

use super::{account, credentials::Credentials, errors, oauth_tokens, util};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope, Snapshot},
    Error, Result, Value,
};

const SCOPE: &str = "control-refresh";
const KEY: &str = "rotation";
const TTL_MS: i64 = 30 * 60 * 1000;

fn snapshot() -> Result<Snapshot> {
    let snapshot = state::conversation_get(SCOPE, KEY, 1)?;
    if !snapshot.compatible {
        return Err(Error::http(409, "续期状态格式已变化，请重新网页登录"));
    }
    Ok(snapshot)
}
fn put(previous: &Snapshot, value: Value) -> Result<()> {
    if !state::conversation_compare_exchange(SCOPE, KEY, 1, previous.revision, value, TTL_MS)? {
        return Err(Error::http(409, "OfficeAce 续期状态已变化，未覆盖其他操作"));
    }
    Ok(())
}
fn remember_done(latest: &Value) {
    if let Ok(previous) = snapshot() {
        let _ = put(
            &previous,
            json!({ "status": "done",
            "next": util::fingerprint(&util::text(&latest["secret"], &["refreshToken", "refresh_token"])), "at": sdk::now() }),
        );
    }
}

fn durable() -> Result<Snapshot> {
    let value = state::get(Scope::Account, "refresh-attempt", 1)?;
    if !value.compatible {
        return Err(uncertain());
    }
    Ok(value)
}

fn durable_put(previous: &Snapshot, value: Value) -> Result<()> {
    if !state::compare_exchange(
        Scope::Account,
        "refresh-attempt",
        1,
        previous.revision,
        value,
        0,
    )? {
        return Err(uncertain());
    }
    Ok(())
}

fn uncertain() -> Error {
    let mut error = Error::http(409, "OfficeAce 上一轮一次性续期结果未确认，未重复消费令牌；请重新网页登录，模型 Basic 凭据仍保留")
        .with_code(errors::REFRESH_UNCERTAIN);
    error.credential_scope = Some("oauth".into());
    error
}

fn bundle(original: &Value, fresh: &Value) -> Value {
    let mut next = original.clone();
    for key in [
        "accessKeyId",
        "secretAccessKey",
        "securityToken",
        "expiresAt",
        "expiresAtEstimated",
        "refreshToken",
    ] {
        next[key] = fresh[key].clone();
    }
    if let Some(project) = fresh["projectId"]
        .as_str()
        .filter(|project| !project.is_empty())
    {
        next["projectId"] = json!(project);
    }
    next
}

fn commit(mut record: Value, current: &Credentials, fresh: &Value, token: &str) -> Result<Value> {
    for _ in 0..2 {
        let next = bundle(&record["secret"], fresh);
        let saved = account::update(&record, &next);
        let (latest, latest_credentials) = account::snapshot()?;
        if latest["id"] != record["id"]
            || latest["identity"] != record["identity"]
            || !latest_credentials.same_model(current)
            || latest["secret"]["dpopKeyPair"] != record["secret"]["dpopKeyPair"]
        {
            return Err(uncertain());
        }
        if util::text(&latest["secret"], &["refreshToken", "refresh_token"])
            == fresh["refreshToken"].as_str().unwrap_or("")
            && latest_credentials.same_control(&Credentials::from_secret(&next))
        {
            return Ok(latest);
        }
        let latest_token = util::text(&latest["secret"], &["refreshToken", "refresh_token"]);
        if latest_token != token
            && !latest_token.is_empty()
            && !current.same_control(&latest_credentials)
        {
            return Ok(latest);
        }
        if util::text(&latest["secret"], &["refreshToken", "refresh_token"]) != token
            || latest["secret"]["dpopKeyPair"] != record["secret"]["dpopKeyPair"]
            || account::identity(&latest["secret"]) != account::identity(&record["secret"])
        {
            return Err(uncertain());
        }
        if saved.is_err() {
            return Err(uncertain());
        }
        record = latest;
    }
    Err(uncertain())
}

/// 调用方必须已持有账号锁；基于拿锁前的快照消除并发重复刷新。
pub fn locked(record: Value, rejected: &Credentials, before_refresh: &str) -> Result<Value> {
    let current = Credentials::from_secret(&record["secret"]);
    let token = util::text(&record["secret"], &["refreshToken", "refresh_token"]);
    if !current.same_model(rejected) {
        return Err(errors::account_changed());
    }
    if !current.same_control(rejected) || token != before_refresh {
        return Ok(record);
    }
    if token.is_empty() {
        return Err(errors::control_scope(Error::http(
            400,
            "此 OfficeAce 账号没有刷新令牌，请网页登录补齐控制面授权",
        )));
    }
    let pair = &record["secret"]["dpopKeyPair"];
    super::dpop::DpopKey::from_json(pair)?;
    let mark = util::fingerprint(&token);
    let key = util::fingerprint(&pair.to_string());
    let proof_context = durable()?;
    let journal = snapshot()?;
    if proof_context.value["previous"] == mark
        && proof_context.value["key"] == key
        && !(journal.value["status"] == "received"
            && journal.value["previous"] == mark
            && journal.value["key"] == key)
    {
        return Err(uncertain());
    }
    if journal.value["status"] == "done"
        && journal.value["next"] == mark
        && sdk::now().saturating_sub(journal.value["at"].as_i64().unwrap_or(0)) < 5000
    {
        return Ok(record);
    }
    if journal.value["previous"] == mark && journal.value["key"] == key {
        if journal.value["status"] == "received" {
            let latest = commit(record, &current, &journal.value["control"], &token)?;
            remember_done(&latest);
            return Ok(latest);
        }
        if matches!(
            journal.value["status"].as_str(),
            Some("submitting" | "uncertain" | "failed")
        ) {
            return Err(uncertain());
        }
    }
    put(
        &journal,
        json!({ "status": "submitting", "previous": mark, "key": key, "at": sdk::now() }),
    )?;
    // 仅保存旧令牌/密钥的摘要：即使30分钟恢复槽过期或主程序重启，也不重放一次性令牌。
    durable_put(
        &proof_context,
        json!({ "previous": mark, "key": key, "at": sdk::now() }),
    )?;
    let reserved = snapshot()?;
    account::unchanged(&record)?;
    let fresh = match oauth_tokens::refresh(pair, &token) {
        Ok(value) => value,
        Err(mut error) => {
            errors::redact(&mut error, &record["secret"]);
            if errors::permission(&error) {
                // 宿主在出站之前拒绝，令牌未被消费；撤权/代理修复后允许重新尝试。
                let _ = durable().and_then(|previous| durable_put(&previous, Value::Null));
                let _ = put(&reserved, Value::Null);
                return Err(error);
            }
            let _ = put(
                &reserved,
                json!({ "status": if error.code.as_deref() == Some(errors::REFRESH_UNCERTAIN) { "uncertain" } else { "failed" },
                "previous": mark, "key": key, "at": sdk::now() }),
            );
            return Err(error);
        }
    };
    let expected_principal = util::text(&record["secret"], &["principalId"]);
    let actual_principal = util::text(&fresh, &["principalId"]);
    if !expected_principal.is_empty()
        && !actual_principal.is_empty()
        && expected_principal != actual_principal
    {
        let _ = put(
            &reserved,
            json!({ "status": "failed", "previous": mark, "key": key, "at": sdk::now() }),
        );
        return Err(uncertain());
    }
    // 先保留新令牌以便 CAS 写回失败时恢复；不把整份旧 secret（含私钥）放入 conversation。
    put(
        &reserved,
        json!({ "status": "received", "previous": mark, "key": key, "control": fresh, "at": sdk::now() }),
    )?;
    let latest = commit(record, &current, &fresh, &token)?;
    remember_done(&latest);
    Ok(latest)
}
