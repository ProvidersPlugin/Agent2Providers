//! 一次性续期材料在发送前持久预约；响应或写回未确认时不重用原刷新令牌。

use super::credentials::Credential;
use agent2api_wasm_sdk::{self as sdk, json, state::{self, Scope, Snapshot}, Error, Result};

const KEY: &str = "credential-refresh";

fn snapshot() -> Result<Snapshot> {
    let current = state::get(Scope::Account, KEY, 1)?;
    if !current.compatible {
        return Err(Error::http(409, "小浣熊续期状态格式已变化，请重新导入有效凭据"));
    }
    Ok(current)
}

pub fn unresolved() -> Error {
    Error::http(409, "小浣熊上次续期结果尚未确认，不会重复使用旧刷新令牌；请重新登录或导入官方客户端当前凭据")
        .with_code("raccoon_refresh_unconfirmed")
}

pub fn available(credential: &Credential) -> Result<bool> {
    Ok(snapshot()?.value["stamp"] != credential.stamp())
}

pub fn complete(credential: &Credential) -> Result<()> {
    let previous = snapshot()?;
    if previous.value["stamp"] != credential.stamp()
        || !state::compare_exchange(Scope::Account, KEY, 1, previous.revision, sdk::Value::Null, 0)? {
        return Err(unresolved());
    }
    Ok(())
}

pub fn reserve(credential: &Credential) -> Result<()> {
    let previous = snapshot()?;
    let stamp = credential.stamp();
    if previous.value["stamp"] == stamp {
        return Err(unresolved());
    }
    if !state::compare_exchange(Scope::Account, KEY, 1, previous.revision,
        json!({ "stamp": stamp, "at": sdk::now() }), 0)? {
        return Err(Error::http(409, "小浣熊续期状态已变化，请重新刷新"));
    }
    Ok(())
}
