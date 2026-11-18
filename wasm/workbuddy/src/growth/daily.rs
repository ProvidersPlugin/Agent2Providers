//! 国内成长活跃每天一条，和daily-checkin两套连签；上报成功后持久按日去重。
use super::GrowthAccount;
use crate::{ledger, util};
use agent2api_wasm_sdk::{self as sdk, json, Value};

pub fn run(account: &GrowthAccount) -> Result<Value, String> {
    let today = util::day(sdk::now());
    let previous = ledger::get(&account.session, "growth-daily").map_err(|e| e.message)?;
    if previous["day"] == today {
        return Ok(
            json!({ "reported": false, "streakDays": util::integer(&previous["streakDays"]).filter(|days| *days > 0), "error": null }),
        );
    }
    super::events::report_chat_send(account, false)?;
    let days = super::growth_streak(account).filter(|days| *days > 0);
    let mut result = json!({ "reported": true, "streakDays": days, "error": null });
    if ledger::put(
        &account.session,
        "growth-daily",
        &json!({ "day": today, "at": sdk::now(), "streakDays": days }),
    )
    .is_err()
    {
        util::warning(&mut result, "活跃已上报，但按日台账未保存");
    }
    Ok(result)
}
