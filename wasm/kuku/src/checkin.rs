//! 最新原生30天签到+旧freepoint串行入口；所有写操作仅在checkin.claim中发生。
//! 不从对话/目录/积分触发签到，不推断今日成功，不造积分、天数或发不存在的领取RPC。

mod freepoint;
mod thirty;
mod token;
mod token_text;

use crate::{credentials::Credentials, errors, session, util};
use agent2api_wasm_sdk::{
    json,
    state::{self, Scope},
    AccountLock, Result, Value,
};

pub(super) fn day() -> Result<i64> {
    Ok((util::now()? + 8 * 60 * 60 * 1000) / 86_400_000)
}
pub(super) fn binding(c: &Credentials) -> String {
    if c.uid.is_empty() {
        util::hash(c.get("BDUSS"))
    } else {
        util::hash(&format!("uid:{}", c.uid))
    }
}

/// 仅记录本次真实对话已完成；不在对话路径向上游上报或领取任何任务。
pub(crate) fn record_chat(c: &Credentials) -> Result<()> {
    let _lock = AccountLock::acquire()?;
    session::unchanged(c)?;
    let before = state::get(Scope::Account, "kuku.chat.completed", 1)?;
    if !before.compatible {
        return Ok(());
    }
    let _ = state::compare_exchange(
        Scope::Account,
        "kuku.chat.completed",
        1,
        before.revision,
        json!({"day":day()?,"binding":binding(c)}),
        0,
    )?;
    Ok(())
}

pub fn report(outcome: &str, message: &str) -> Value {
    json!({ "outcome": outcome, "credit": null, "streak": null, "message": message })
}
pub fn claim() -> Result<Value> {
    let _lock = AccountLock::acquire()?;
    let (_, c) = session::read()?;
    let result = match thirty::claim(&c) {
        Ok(Some(result)) => Ok(result),
        Ok(None) => freepoint::claim_locked(),
        Err(error) => Err(error),
    };
    match result {
        Ok(value) => Ok(value),
        Err(error) if errors::host_stop(&error) => Err(error),
        Err(error) => Ok(report(
            "failed",
            &errors::safe_message(&error.message, &c.secrets()),
        )),
    }
}
