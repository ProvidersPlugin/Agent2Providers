//! 复用旧台账名；CAS持久状态是新写入依据，旧ledger只做迁移读取与兼容镜像。
//! save/remember调用方必须持账号锁；get允许只读预览并以CAS迁移旧状态。
//! 敏感兑换码仅以旧SHA-256短摘要进键，不写原文。

use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{Scope, Snapshot},
    Error, Result, Value,
};

pub fn get(key: &str) -> Result<Snapshot> {
    let current = sdk::state::get(Scope::Account, key, 1)?;
    if !current.compatible {
        return Err(Error::http(409, "QuWork 奖励记录版本不兼容，未提交领取"));
    }
    if current.revision > 0 || !current.value.is_null() {
        return Ok(current);
    }
    let old = sdk::host("ledger.get", json!({ "kind": key }))?;
    if old.is_null() {
        return Ok(current);
    }
    if !sdk::state::compare_exchange(Scope::Account, key, 1, current.revision, old, 0)? {
        return sdk::state::get(Scope::Account, key, 1);
    }
    sdk::state::get(Scope::Account, key, 1)
}

pub fn save(key: &str, snapshot: &Snapshot, value: Value) -> Result<()> {
    if !snapshot.compatible
        || !sdk::state::compare_exchange(
            Scope::Account,
            key,
            1,
            snapshot.revision,
            value.clone(),
            0,
        )?
    {
        return Err(Error::http(409, "QuWork 奖励记录已变化，未重复提交"));
    }
    // 写请求前这一步同样必须成功；失败时CAS中保留confirming，不能继续请求上游。
    sdk::host("ledger.put", json!({ "kind": key, "value": value }))?;
    Ok(())
}

pub fn remember(key: &str, value: Value) -> Result<()> {
    save(key, &get(key)?, value)
}
