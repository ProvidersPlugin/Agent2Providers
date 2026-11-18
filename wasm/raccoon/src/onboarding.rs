//! 两条真实首登奖励与旧onboardingGrants/onboarding台账；状态查询零上游，名义各3000。
//! granted=false也是已结算；granted=true但金额缺失只报已发放，不把名义3000伪装成本次实收。

use super::{account, config, ledger, network, session, util};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

const GRANTS: &str = "onboardingGrants";
const SETTLED: &str = "onboarding";
const TASKS: [(&str, &str, &str, bool); 2] = [
    (
        "desktop_login_grant",
        "首次电脑端登录奖励",
        "/api/web/desktop/v1/login/points/grant",
        false,
    ),
    (
        "mobile_login_grant",
        "首次手机端登录奖励",
        "/api/web/mobile/v1/login/points/grant",
        true,
    ),
];
fn load(binding: &str) -> Result<Value> {
    let value = ledger::scoped(GRANTS, binding)?;
    if value.is_null() {
        Ok(json!({}))
    } else if value.is_object() {
        Ok(value)
    } else {
        Err(Error::http(
            502,
            "小浣熊首次登录台账格式无效，未当成从未领取",
        ))
    }
}
fn document(ledger: &Value) -> Value {
    let rows: Vec<Value> = TASKS.iter().map(|(key, title, _, _)| json!({ "key": key, "title": title,
        "group": "初识小浣熊", "points": 3000, "done": ledger[*key].as_i64().is_some_and(|at| at > 0) })).collect();
    let missing = rows.iter().filter(|row| row["done"] != true).count();
    let at = TASKS
        .iter()
        .filter_map(|(key, _, _, _)| ledger[*key].as_i64())
        .max()
        .unwrap_or(0);
    json!({ "tasks": rows, "earned": (2 - missing) * 3000, "total": 6000, "unclaimed": missing,
        "settled": missing == 0, "settledAt": (missing == 0).then_some(at),
        "note": "任务分值为官方名义值，实际新增积分只使用领取响应读数；未落台账表示尚未确认，不代表官方一定没发过" })
}
fn remember(result: &mut Value, binding: &str) -> Result<()> {
    ledger::check_binding(binding)?;
    let value = if result["settled"] == true {
        json!({ "at": result["settledAt"], "tasks": result["tasks"], "earned": result["earned"], "total": result["total"] })
    } else {
        Value::Null
    };
    if ledger::scoped_put(SETTLED, &value, binding).is_err() {
        util::warning(result, "新手任务结算快照未保存，本次官方结果不受影响");
    }
    ledger::check_binding(binding)
}
pub fn status() -> Result<Value> {
    account::record()?;
    let _lock = AccountLock::acquire()?;
    let binding = ledger::binding()?;
    let mut result = document(&load(&binding)?);
    remember(&mut result, &binding)?;
    Ok(result)
}
pub fn claim() -> Result<Value> {
    // 已结清时不必解析/续期凭据，更不能意外读取桌面文件。
    account::record()?;
    let lock = AccountLock::acquire()?;
    let binding = ledger::binding()?;
    let mut old = load(&binding)?;
    if document(&old)["settled"] == true {
        let mut result = document(&old);
        result["results"] = json!([]);
        result["claimed"] = json!(0);
        result["failed"] = json!(0);
        result["claimedPoints"] = json!(0);
        remember(&mut result, &binding)?;
        return Ok(result);
    }
    // 领取自身准备凭据，不能假设上层已执行token.ensure；复用当前锁，不嵌套申请。
    let current = session::ensure_locked(&lock)?;
    ledger::check_binding(&binding)?;
    if ledger::binding_of(&current.record) != binding {
        return Err(Error::http(409, "小浣熊领取前账号身份已变化，请重新发起"));
    }
    let mut results = Vec::new();
    let mut count = 0;
    let mut failed = 0;
    let mut points = Some(0i64);
    let mut warnings = Vec::new();
    for (key, _, path, mobile) in TASKS {
        if old[key].as_i64().is_some_and(|at| at > 0) {
            continue;
        }
        ledger::check_snapshot(&current)?;
        let reply = network::request(
            "POST",
            path,
            &current.credential.token,
            &current.credential.org,
            config::page(mobile)?,
            None,
        )
        .and_then(|value| {
            value["granted"]
                .as_bool()
                .map(|granted| {
                    (
                        granted,
                        util::integer(&value["popup"]["points"]).filter(|points| *points > 0),
                    )
                })
                .ok_or_else(|| Error::http(502, "首登奖励响应缺少granted，未确认领取"))
        });
        match reply {
            Ok((granted, amount)) => {
                count += 1;
                let amount = if granted { amount } else { Some(0) };
                points = points
                    .zip(amount)
                    .map(|(left, right)| left.saturating_add(right));
                ledger::check_snapshot(&current)?;
                old[key] = json!(sdk::now());
                if ledger::scoped_put(GRANTS, &old, &binding).is_err() {
                    warnings.push("首登奖励已结算，但逐项台账未保存".to_string());
                }
                results
                    .push(json!({ "key": key, "ok": true, "already": !granted, "points": amount }));
            }
            Err(error) if error.status == Some(401) => return Err(error),
            Err(error) => {
                failed += 1;
                results.push(json!({ "key": key, "ok": false, "error": error.message }));
            }
        }
    }
    let mut result = document(&old);
    result["results"] = json!(results);
    result["claimed"] = json!(count);
    result["failed"] = json!(failed);
    result["claimedPoints"] = json!(points);
    if points.is_none() {
        warnings.push("官方已确认发放，但部分奖励没有金额读数，未用名义积分代替实收".into());
    }
    if !warnings.is_empty() {
        result["warnings"] = json!(warnings);
    }
    ledger::check_snapshot(&current)?;
    remember(&mut result, &binding)?;
    Ok(result)
}
