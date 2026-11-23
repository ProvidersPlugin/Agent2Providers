//! 用户资料接口不是活动触发接口；source_id 按官方 withWebInfo 保留 autotyper。
use super::{account, client, util};
use agent2api_wasm_sdk::{json, AccountLock, Error, Result, Value};

pub fn fetch(secret: &Value) -> Result<Value> {
    let payload = client::post(
        "/userapi/v1/user-profile",
        &account::access(secret),
        &json!({ "source_id": "autotyper", "device_id": account::device(secret) }),
        "用户资料查询",
    )?;
    client::business(&payload, "查询用户资料")?;
    if !payload["data"].is_object() {
        return Err(Error::http(502, "AutoClaw 国际版资料响应缺少有效内容"));
    }
    let uid = util::scalar(&payload["data"]["user_id"]);
    if !uid.is_empty() && !util::printable(&uid, 256) {
        return Err(Error::http(502, "AutoClaw 国际版资料用户标识无效"));
    }
    if !uid.is_empty() && !account::user(secret).is_empty() && uid != account::user(secret) {
        return Err(Error::http(409, "AutoClaw 国际版资料与当前账号身份不一致")
            .with_code("identity_changed"));
    }
    Ok(json!({ "userId": uid, "email": util::text(&payload["data"], &["email"]) }))
}

pub fn fill_email(snapshot: &account::Snapshot) -> Result<()> {
    if snapshot.local || !util::text(&snapshot.secret, &["email"]).is_empty() {
        return Ok(());
    }
    let email = util::text(&fetch(&snapshot.secret)?, &["email"]);
    if email.is_empty() {
        return Ok(());
    }
    let _lock = AccountLock::acquire()?;
    let current = account::read()?;
    if !account::same(snapshot, &current) {
        return Ok(());
    }
    let mut next = account::decode(&current.record["secret"])?;
    if !util::text(&next, &["email"]).is_empty() {
        return Ok(());
    }
    next["email"] = json!(email);
    account::save(&current, next).map(|_| ())
}
