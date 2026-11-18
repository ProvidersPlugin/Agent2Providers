//! 台账经当前账号宿主回调；首次接管保存旧形状，后续按平台uid/组织/桌面来源隔离。
//! 账号state保留带绑定的权威副本，兼容ledger仍镜像原形状；镜像失败不能把旧台账认成新账号。

use super::{account, session::Session, util};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    Error, Result, Value,
};

fn get(kind: &str) -> Result<Value> {
    sdk::host("ledger.get", json!({ "kind": kind }))
}
fn put(kind: &str, value: &Value) -> Result<()> {
    sdk::host("ledger.put", json!({ "kind": kind, "value": value })).map(|_| ())
}

pub fn binding_of(record: &Value) -> String {
    let secret = &record["secret"];
    let org = util::text(secret, &["officeIdentity", "office_identity"]);
    util::hash(
        &json!({ "account": record["id"], "user": secret["userId"],
        "identity": account::identity(secret), "org": if org == "personal" { "" } else { &org },
        "desktop": secret["desktop"] == true, "source": secret["sourceId"] })
        .to_string(),
    )
}
pub fn binding() -> Result<String> {
    Ok(binding_of(&account::record()?))
}
pub fn check_binding(expected: &str) -> Result<()> {
    if binding()? != expected {
        return Err(changed());
    }
    Ok(())
}
fn changed() -> Error {
    Error::http(409, "小浣熊账号或来源已变化，未把本次结果记给另一账号")
        .with_code("ledger_identity_changed")
}
pub fn check_snapshot(session: &Session) -> Result<()> {
    check_binding(&binding_of(&session.record))?;
    // 官方客户端可以在宿主账号引用不变时切号，因此桌面请求还须重新核对当前来源。
    if session.source.is_some() {
        let latest = super::session::snapshot()?;
        if latest.credential.user != session.credential.user
            || latest.credential.org != session.credential.org
            || latest.source.as_ref().map(|source| &source.source_id)
                != session.source.as_ref().map(|source| &source.source_id)
        {
            return Err(changed());
        }
    }
    Ok(())
}

/// 首次只读也接管绑定；状态查询仍然零上游、零桌面helper读取。
pub fn scoped(kind: &str, expected: &str) -> Result<Value> {
    let key = format!("ledger-{kind}");
    for _ in 0..3 {
        check_binding(expected)?;
        let snapshot = state::get(Scope::Account, &key, 1)?;
        if !snapshot.compatible {
            return Err(Error::http(409, "小浣熊台账来源格式已变化"));
        }
        if snapshot.value["binding"]
            .as_str()
            .is_some_and(|known| known != expected)
        {
            return Ok(Value::Null);
        }
        if let Some(value) = snapshot.value.get("value") {
            check_binding(expected)?;
            return Ok(value.clone());
        }
        let legacy = get(kind)?;
        check_binding(expected)?;
        if state::compare_exchange(
            Scope::Account,
            &key,
            1,
            snapshot.revision,
            json!({ "binding": expected, "value": legacy }),
            0,
        )? {
            check_binding(expected)?;
            return Ok(legacy);
        }
    }
    Err(Error::http(409, "小浣熊台账正在更新，请重新读取"))
}

pub fn scoped_put(kind: &str, value: &Value, expected: &str) -> Result<()> {
    check_binding(expected)?;
    let key = format!("ledger-{kind}");
    let snapshot = state::get(Scope::Account, &key, 1)?;
    if !snapshot.compatible {
        return Err(Error::http(409, "小浣熊台账来源格式已变化"));
    }
    check_binding(expected)?;
    if !state::compare_exchange(
        Scope::Account,
        &key,
        1,
        snapshot.revision,
        json!({ "binding": expected, "value": value }),
        0,
    )? {
        return Err(Error::http(409, "小浣熊台账已被其他请求更新"));
    }
    check_binding(expected)?;
    put(kind, value)?;
    check_binding(expected)
}
