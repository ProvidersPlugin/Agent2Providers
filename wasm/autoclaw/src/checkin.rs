//! daily_signin 由 task-complete 完成，不把 HTTP 200 或已完成当新奖励。
//! 账号锁 + 在途台账避免并发/取消后的立即重领；活动奖励仍以 user-info 触发及流水确认。
use super::{account, client, endpoints, promotion, util};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

const KIND: &str = "autoclawDailySignin";
const TASK_COMPLETE: &str = "/autoclaw-proxy/proxy/autoclaw-task-complete";
const TASK_LIST: &str = "/autoclaw-proxy/proxy/autoclaw-task-list";

fn save(value: &Value) -> Result<()> {
    sdk::host("ledger.put", json!({ "kind": KIND, "value": value })).map(|_| ())
}
fn identity(snapshot: &account::Snapshot) -> String {
    let user = account::user(&snapshot.secret);
    util::hash(
        &json!([
            endpoints::PROVIDER,
            snapshot.record["id"],
            if user.is_empty() {
                util::hash(&account::access(&snapshot.secret))
            } else {
                user
            }
        ])
        .to_string(),
    )
}

pub fn claim() -> Result<Value> {
    let initial = account::read()?;
    let _lock = AccountLock::acquire()?;
    let current = account::read()?;
    if !account::same(&initial, &current) {
        return Err(Error::http(409, "AutoClaw 账号已变化，未执行签到"));
    }
    let day = util::day()?;
    let id = identity(&current);
    let mut ledger = sdk::host("ledger.get", json!({ "kind": KIND }))?;
    if !ledger.is_null() && !ledger.is_object() {
        return Err(Error::http(409, "AutoClaw 签到台账格式异常，未执行签到"));
    }
    if !ledger.is_object() || ledger["day"] != day || ledger["identity"] != id {
        ledger = json!({ "day": day, "identity": id, "completed": false });
    }
    if ledger["inFlightUntil"].as_i64().unwrap_or(0) > sdk::now() {
        return Ok(report(
            &json!({ "success": false, "msg": "上一轮签到结果尚未确认，稍后会核对任务状态", "pending": true }),
            Some(
                ledger["inFlightUntil"]
                    .as_i64()
                    .unwrap_or(sdk::now() + 60_000),
            ),
        ));
    }
    if ledger["retryAt"].as_i64().unwrap_or(0) > sdk::now() {
        return Ok(report(
            &json!({ "success": false, "msg": "本轮签到尚在冷却，稍后继续核对", "pending": true }),
            ledger["retryAt"].as_i64(),
        ));
    }
    let mut claim = if ledger["completed"] == true {
        json!({ "success": false, "alreadyCompleted": true, "msg": "今天已签到" })
    } else {
        // 崩溃留下在途标记后，先用只读任务状态核对，不直接重复 POST。
        let confirmed = if ledger["inFlightUntil"].as_i64().unwrap_or(0) > 0 {
            match task_done(&current.secret)? {
                Some(completed) => completed,
                None => {
                    return Ok(report(
                        &json!({ "success": false, "msg": "上一轮签到结果未知，任务列表也未给出结论，请稍后核对", "pending": true }),
                        Some(sdk::now() + 600_000),
                    ))
                }
            }
        } else {
            false
        };
        if confirmed {
            json!({ "success": false, "alreadyCompleted": true, "msg": "任务列表确认今天已签到" })
        } else {
            ledger["inFlightUntil"] = json!(sdk::now() + 120_000);
            save(&ledger)?;
            run(&current.secret)?
        }
    };
    ledger["completed"] = json!(claim["success"] == true || claim["alreadyCompleted"] == true);
    // POST 已发出但任务状态未知时保留在途标记；冷却后只能先回读，不能直接重领。
    if ledger["completed"] == true || claim["pending"] != true {
        ledger["inFlightUntil"] = json!(0);
    }
    ledger["retryAt"] = if ledger["completed"] == true {
        Value::Null
    } else {
        json!(sdk::now() + 600_000)
    };
    ledger["updatedAt"] = json!(sdk::now());
    let current_again = account::read()?;
    if !account::same(&current, &current_again) {
        return Err(Error::http(
            409,
            "AutoClaw 账号已变化，未把旧签到结果写到新身份",
        ));
    }
    save(&ledger)?;

    let mut reward = match promotion::today(&current.secret) {
        Ok(value) => value,
        Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
        Err(_) => Value::Null,
    };
    // 只有确实读到「未发放」才触发；查不到不能推断无奖励并重复副作用。
    if reward["granted"] == false
        && sdk::now().saturating_sub(ledger["promotionAttemptAt"].as_i64().unwrap_or(0)) >= 600_000
    {
        if !account::same(&current, &account::read()?) {
            return Err(Error::http(409, "AutoClaw 账号已变化，未触发旧账号活动"));
        }
        ledger["promotionAttemptAt"] = json!(sdk::now());
        save(&ledger)?;
        match promotion::activate(&current.secret) {
            Ok(()) => {
                sdk::sleep(1500)?;
                match promotion::today(&current.secret) {
                    Ok(value) => reward = value,
                    Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
                    Err(_) => {}
                }
            }
            Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
            Err(error) => {
                claim["promotionError"] = json!(error.message);
            }
        }
    }
    if !account::same(&current, &account::read()?) {
        return Err(Error::http(
            409,
            "AutoClaw 账号已变化，请重新查询签到与活动结果",
        ));
    }
    claim = promotion::attach(claim, reward);
    let retry = if claim["success"] == true || claim["alreadyCompleted"] == true {
        None
    } else {
        Some(sdk::now() + 600_000)
    };
    Ok(report(&claim, retry))
}

fn run(secret: &Value) -> Result<Value> {
    let payload = client::post(
        TASK_COMPLETE,
        &account::access(secret),
        &json!({ "task_id": "daily_signin" }),
        "签到",
    )?;
    client::business(&payload, "签到")?;
    let data = &payload["data"];
    if !data.is_object() {
        return Err(Error::http(502, "AutoClaw 签到未返回有效任务结果"));
    }
    let points = util::integer(&data["reward_points"]);
    let already = data["already_completed"] == true;
    let success = !already && (points.is_some_and(|points| points > 0) || data["success"] == true);
    if success {
        return Ok(
            json!({ "success": true, "alreadyCompleted": false, "rewardPoints": points,
            "msg": points.filter(|points| *points > 0).map(|points| format!("签到成功，获得 {points} 积分")).unwrap_or_else(|| "签到成功".into()) }),
        );
    }
    if already {
        return Ok(
            json!({ "success": false, "alreadyCompleted": true, "rewardPoints": points, "msg": "今天已签到" }),
        );
    }
    let completed = match task_done(secret) {
        Ok(value) => value,
        Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
        Err(_) => None,
    };
    Ok(
        json!({ "success": false, "alreadyCompleted": completed == Some(true), "pending": completed.is_none(), "rewardPoints": points,
        "msg": match completed { Some(true) => "任务列表显示今天已签到", Some(false) => "签到未领取，稍后重试",
            None => "签到结果尚未确认，稍后只读核对任务状态" } }),
    )
}
fn task_done(secret: &Value) -> Result<Option<bool>> {
    let payload = client::get(TASK_LIST, &account::access(secret), "任务列表查询")?;
    client::business(&payload, "查询任务列表")?;
    Ok(payload["data"]
        .as_array()
        .and_then(|tasks| tasks.iter().find(|task| task["task_id"] == "daily_signin"))
        .and_then(|task| task["completed"].as_bool()))
}
fn report(claim: &Value, retry: Option<i64>) -> Value {
    let outcome = if claim["success"] == true {
        "claimed"
    } else if claim["alreadyCompleted"] == true {
        "done"
    } else if claim["pending"] == true {
        "inactive"
    } else {
        "failed"
    };
    let credit = (claim["success"] == true)
        .then(|| util::integer(&claim["rewardPoints"]))
        .flatten()
        .filter(|points| *points > 0)
        .map(|points| format!("{points} 积分"));
    json!({ "outcome": outcome, "message": util::text(claim, &["msg"]), "credit": credit, "streak": null,
        "retryAt": retry, "details": claim })
}
