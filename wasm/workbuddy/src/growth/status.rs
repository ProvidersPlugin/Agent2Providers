//! 成长状态四块独立只读；某块失败只返回该块错误，不把空值报成0余额。
use super::{list_tasks, school, spec, travel, GrowthAccount};
use agent2api_wasm_sdk::{json, Value};

fn row(task: &Value) -> Value {
    let code = task["task_code"].as_str().unwrap_or("");
    let spec = spec::spec_of(code);
    json!({ "code": code, "title": task["title"], "description": task["description"], "status": task["accept_status"],
        "current": task["progress"]["current"], "target": task["progress"]["target"], "locked": task["locked"] == true,
        "unlockAt": task["unlock_at"], "supported": spec.is_some(), "unforgeable": spec.as_ref().is_some_and(|s| s.unforgeable),
        "unforgeableReason": spec.as_ref().map(|s| s.reason).unwrap_or(""), "kind": spec.as_ref().map(|s| s.kind).unwrap_or(""), "source": spec.as_ref().map(|s| s.src).unwrap_or("") })
}
pub fn query(account: &GrowthAccount) -> Result<Value, String> {
    let tasks = match list_tasks(account, false) {
        Ok(tasks) => {
            let mut rows: Vec<_> = tasks.iter().map(row).collect();
            if let Ok(mp) = list_tasks(account, true) {
                for task in mp {
                    if rows.iter().any(|r| r["code"] == task["task_code"]) {
                        continue;
                    }
                    rows.push(row(&task));
                }
            }
            json!({ "tasks": rows, "error": null })
        }
        Err(error) => json!({ "tasks": [], "error": error }),
    };
    let energy = super::energy_balance(account);
    let streak = super::growth_streak(account);
    let travel = match travel::info(account) {
        Ok(Some(buddy)) => match travel::state(account) {
            Ok(state) => {
                json!({ "hasBuddy": true, "buddyName": buddy["name"], "state": state["state"], "dailyLimitReached": state["daily_limit_reached"], "recordId": state["record_id"], "rewardCredit": state["reward_credit"], "error": null })
            }
            Err(error) => json!({ "hasBuddy": true, "buddyName": buddy["name"], "error": error }),
        },
        Ok(None) => json!({ "hasBuddy": false, "buddyName": "", "state": "", "error": null }),
        Err(error) => json!({ "hasBuddy": null, "error": error }),
    };
    let school = match school::tasks(account) {
        Ok((tasks, active)) => {
            let chance = school::lottery(account).ok().map(|v| json!({ "balance": v["chance"]["balance"], "totalEarned": v["chance"]["total_earned"] }));
            let rows: Vec<_> = tasks.iter().map(|task| json!({ "code": task["task_code"], "title": task["title"], "status": task["status"], "current": task["progress"], "target": task["target_count"], "rewardCredit": task["reward_credit"] })).collect();
            json!({ "inPeriod": active, "tasks": rows, "lottery": chance, "error": null })
        }
        Err(error) => json!({ "inPeriod": null, "tasks": [], "lottery": null, "error": error }),
    };
    account.check()?;
    Ok(
        json!({ "id": account.session.record["id"], "uid": account.uid, "nick": account.nick, "energy": energy, "streak": streak, "tasks": tasks, "travel": travel, "school": school }),
    )
}
