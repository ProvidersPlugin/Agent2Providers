//! 用户资料接口不是活动触发接口；source_id 按官方 withWebInfo 保留 autotyper。
use super::{account, client, util};
use agent2api_wasm_sdk::{json, AccountLock, Result, Value};

pub fn fetch(secret: &Value) -> Result<Value> {
    let payload = client::post(
        "/userapi/v1/user-profile",
        &account::access(secret),
        &json!({ "source_id": "autotyper", "device_id": account::device(secret) }),
        "用户资料查询",
    )?;
    client::business(&payload, "查询用户资料")?;
    Ok(
        json!({ "userId": util::scalar(&payload["data"]["user_id"]), "email": util::text(&payload["data"], &["email"]) }),
    )
}

pub fn fill_email(snapshot: &account::Snapshot) -> Result<()> {
    if snapshot.local || !util::text(&snapshot.secret, &["email"]).is_empty() {
        return Ok(());
    }
    let profile = fetch(&snapshot.secret)?;
    let expected = account::user(&snapshot.secret);
    let actual = util::text(&profile, &["userId"]);
    if !expected.is_empty() && !actual.is_empty() && expected != actual {
        return Err(agent2api_wasm_sdk::Error::http(
            409,
            "AutoClaw 用户资料身份已变化，未回填另一账号邮箱",
        )
        .with_code("identity_changed"));
    }
    let email = util::text(&profile, &["email"]);
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
