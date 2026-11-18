//! 成长动作仅当前账号；旧批量由宿主tasks逐账号分发，WASM不读取整库或后台持活。
use crate::{
    billing,
    growth::{self, GrowthAccount},
    ledger, session,
};
use agent2api_wasm_sdk::{json, AccountLock, Error, Result, Value};

pub fn dispatch(params: &Value) -> Result<Value> {
    dispatch_with_budget(params, 45_000)
}

pub fn scheduled(params: &Value) -> Result<Value> {
    dispatch_with_budget(params, 480_000)
}

// 手动管理RPC为120秒、桌面转发为60秒；只在45秒内启动下一步，仍不能中断在途HTTP。
// tasks.run由宿主提供600秒，包内480秒预算给结果和台账留余量。
fn dispatch_with_budget(params: &Value, budget_ms: i64) -> Result<Value> {
    let until = agent2api_wasm_sdk::now().saturating_add(budget_ms);
    let action = params["action"].as_str().unwrap_or("");
    let payload = &params["payload"];
    match action {
        "growth.cancel" => return ledger::cancel(),
        "growth.run-status" => return ledger::status(),
        "billing.checkin-claim" => return billing::checkin::claim(),
        "billing.reward" => return billing::rewards::claim(payload["kind"].as_str().unwrap_or("")),
        _ => {}
    }
    if matches!(action, "growth.travel.batch" | "growth.streak.batch")
        && params["account"].as_str().unwrap_or("").is_empty()
    {
        return Err(Error::http(
            501,
            "WASM批量成长任务须由主程序按账号派发tasks.run，不能在插件内枚举全库",
        )
        .with_code("host_account_task_dispatch_required"));
    }
    let _ = session::ensure(false)?;
    if action.starts_with("billing.") {
        let current = session::snapshot()?;
        return match action {
            "billing.checkin-status" => billing::checkin::status(&current),
            "billing.banner" => billing::rewards::banner(&current),
            "billing.ambassador" => billing::rewards::ambassador(&current),
            _ => Err(Error::unsupported()),
        };
    }
    if action == "growth.status" {
        return growth::status::query(&GrowthAccount::new(session::snapshot()?, None, until)?)
            .map_err(|s| Error::http(502, s));
    }
    if !matches!(
        action,
        "growth.task"
            | "growth.school"
            | "growth.travel"
            | "growth.travel.batch"
            | "growth.streak"
            | "growth.streak.batch"
            | "growth.daily"
    ) {
        return Err(Error::unsupported());
    }
    let lock = AccountLock::acquire()?;
    let current = session::ensure_locked(&lock)?;
    let growth = GrowthAccount::new(current.clone(), Some(action), until)?;
    if payload["confirmAgreement"] == true {
        growth::travel::consent(&growth, true).map_err(|s| Error::http(502, s))?;
    }
    let result = match action {
        "growth.task" => growth::run_task(&growth, payload),
        "growth.school" => growth::school::run(&growth, payload).map_err(|s| Error::http(502, s)),
        "growth.travel" | "growth.travel.batch" => {
            growth::travel::run(&growth).map_err(|s| Error::http(502, s))
        }
        "growth.streak" | "growth.streak.batch" => {
            growth::streak::run(&growth).map_err(|s| Error::http(502, s))
        }
        "growth.daily" => growth::daily::run(&growth).map_err(|s| Error::http(502, s)),
        _ => Err(Error::unsupported()),
    };
    let mut report = match result {
        Ok(report) => report,
        Err(error) => {
            let mut report =
                json!({ "outcome": "failed", "message": error.message, "error": error.message });
            ledger::record(&current, action, &mut report);
            return Err(error);
        }
    };
    if !report.is_object() {
        report = json!({ "result": report });
    }
    report["id"] = current.record["id"].clone();
    ledger::record(&current, action, &mut report);
    // growth.daily失败不应让签到主结果失败，宿主保留growth独立结果。
    if action == "growth.daily" && report["reported"] == true && report["streakDays"] == 0 {
        report["streakDays"] = Value::Null;
    }
    Ok(report)
}
