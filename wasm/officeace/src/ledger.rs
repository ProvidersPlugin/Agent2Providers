//! 只使用当前账号台账，保留 officeaceBonus / dailyCheckin / officeaceTrialOrder 的旧键。
//! 写订单前台账失败必须阻止请求；已到账奖励的台账失败只提示，不把领取成功改成失败。

use super::util;
use agent2api_wasm_sdk::{self as sdk, json, Result, Value};

pub const BONUS: &str = "officeaceBonus";
pub const DAILY: &str = "dailyCheckin";
pub const TRIAL: &str = "officeaceTrialOrder";

pub fn get(kind: &str) -> Result<Value> {
    sdk::host("ledger.get", json!({ "kind": kind }))
}
pub fn put(kind: &str, value: &Value) -> Result<()> {
    sdk::host("ledger.put", json!({ "kind": kind, "value": value })).map(|_| ())
}

pub fn today() -> Result<Value> {
    let day = util::day(sdk::now());
    let value = get(BONUS)?;
    Ok(if value["day"] == day {
        value
    } else {
        json!({ "day": day, "items": [], "all": false })
    })
}

pub fn mark_claimed(ids: &[String]) -> Result<()> {
    let mut ledger = today()?;
    let mut items: std::collections::BTreeSet<String> = ledger["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect();
    items.extend(ids.iter().cloned());
    ledger["items"] = json!(items);
    ledger["all"] = json!(true);
    put(BONUS, &ledger)
}

pub fn daily(completed: bool, gained: f64, message: &str) -> Result<()> {
    let old = get(DAILY)?;
    if !completed
        && old["completed"] == true
        && old["at"]
            .as_i64()
            .is_some_and(|at| util::day(at) == util::day(sdk::now()))
    {
        return Ok(());
    }
    put(
        DAILY,
        &json!({ "kind": "daily", "at": sdk::now(), "completed": completed,
        "status": if !completed { "failed" } else if gained > 0.0 { "claimed" } else { "done" },
        "message": message, "completedCount": usize::from(completed), "totalCount": 1, "sourcesComplete": completed }),
    )
}
