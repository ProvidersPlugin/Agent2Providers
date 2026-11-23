//! tasks.v1当前每账号600秒上限、30秒扫描；旅行一趟一步，不在WASM开线程。
//! travel沿旧growthSchedule开关/09:00和21:00；新成长排程默认关闭，不能借迁移自动开启。
use crate::{account, admin, ledger, session};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub fn list() -> Value {
    json!([
        { "id": "travel", "label": "猫猫旅行巡检", "enabled": true, "times": ["09:00", "21:00"], "interval_ms": null,
            "utc_offset_minutes": 480, "checkin": false, "legacy_config": "growthSchedule" },
        { "id": "growth-daily", "label": "成长中心每日活跃", "enabled": false, "times": ["09:05"], "interval_ms": null, "utc_offset_minutes": 480, "checkin": false },
        { "id": "growth-streak", "label": "连登补签与档位奖励", "enabled": false, "times": ["09:10"], "interval_ms": null, "utc_offset_minutes": 480, "checkin": false }
    ])
}
pub fn run(params: &Value) -> Result<Value> {
    let action = match params["task"].as_str().unwrap_or("") {
        "travel" => "growth.travel",
        "growth-daily" => "growth.daily",
        "growth-streak" => "growth.streak",
        _ => return Err(Error::unsupported()),
    };
    let current = session::snapshot()?;
    if !account::personal(&current.secret) {
        return Ok(json!({ "success": true, "message": "企业账号没有个人成长任务，已跳过" }));
    }
    // 宿主默认账号间隔200ms；再等待800ms，保守不高于原生800ms频率。
    sdk::sleep(800)?;
    let at = params["scheduled_at"].as_i64().unwrap_or(0);
    let key = format!("task-{}", params["task"].as_str().unwrap_or(""));
    if params["manual"] != true && at > 0 {
        let old = ledger::get(&current, &key)?;
        if old["scheduledAt"] == at && old["success"] == true {
            return Ok(json!({ "success": true, "message": "本时段已完成巡检" }));
        }
    }
    let result =
        admin::scheduled(&json!({ "account": params["account"], "action": action, "payload": {} }));
    let mut result = match result {
        Ok(report) => {
            json!({ "success": report["outcome"] != "failed" && report["failed"].as_u64().unwrap_or(0) == 0,
            "message": report["message"].as_str().unwrap_or("本轮成长任务已结束"), "report": report, "scheduledAt": at, "at": sdk::now() })
        }
        Err(error) => {
            json!({ "success": false, "message": error.message, "scheduledAt": at, "at": sdk::now() })
        }
    };
    session::check(&current)?;
    if ledger::put(&current, &key, &result).is_err() {
        crate::util::warning(&mut result, "任务已结束，但详细台账未保存");
    }
    Ok(result)
}
