//! 原生status→claim→同设备status回查，空体{}、9074不换方案重试、1001最多续期后一次。
//! 每轮动态生成独立 16 位数字设备号，并在同轮 status→claim→回查间保持一致（复用静态设备号会触发 9074 风控）。
//! 没有新手领取端点证据，不能把查询或登录冒充领取。

use super::{account, client, credentials, endpoints, errors, headers, refresh, util};
use agent2api_wasm_sdk::{self as sdk, json, state::Scope, AccountLock, Error, Result, Value};

const KEY: &str = "daily-checkin";

fn account_binding(secret: &Value) -> String {
    util::digest(
        json!([
            credentials::variant(secret),
            secret["uid"],
            secret["deviceId"],
            secret["machineId"]
        ])
        .to_string()
        .as_bytes(),
    )
}

fn tomorrow(now: i64) -> i64 {
    (now.saturating_add(8 * 3600_000).div_euclid(86_400_000) + 1) * 86_400_000 - 8 * 3600_000
}

fn report(outcome: &str, message: &str, credit: Option<f64>, retry: Option<i64>) -> Value {
    json!({ "outcome": outcome, "message": message, "credit": credit.map(|n| n.to_string()), "streak": null, "retryAt": retry })
}

fn roundtrip(secret: &Value, endpoint: &str, device: &str) -> Result<Value> {
    for bearer in [false, true] {
        let reply = client::post(
            &format!("{}{endpoint}", endpoints::ACCOUNT),
            headers::ug(secret, device, bearer)?,
            &json!({}),
        )?;
        if !(200..300).contains(&reply.status) {
            return Err(errors::upstream(reply.status, &reply.body));
        }
        if !reply.body.is_object() {
            return Err(Error::http(502, "Trae 签到返回格式无效"));
        }
        let code = errors::code(&reply.body)
            .ok_or_else(|| Error::http(502, "Trae 签到返回缺少明确业务状态"))?;
        if code == 0 && !errors::failed(&reply.body) {
            return Ok(reply.body);
        }
        let error = errors::upstream(reply.status, &reply.body);
        // 状态查询可在明确非鉴权业务拒绝后试另一种已知头；领奖只能提交一次。
        if endpoint == endpoints::CHECKIN_CLAIM
            || matches!(code, 9074 | 1001 | 401 | 20101 | 20401 | 20405)
            || bearer
        {
            return Err(error);
        }
    }
    Err(Error::http(502, "Trae 签到未得到可接受的回应"))
}

fn done(value: &Value) -> bool {
    value["checked_in"] == true || value["did_checked_in"] == true
}
fn credit(value: &Value) -> Option<f64> {
    util::number(&value["credits"])
        .filter(|v| *v > 0.0)
        .or_else(|| util::number(&value["extra_credits"]).filter(|v| *v > 0.0))
}

fn run(record: &Value, device: &str) -> Result<Value> {
    let secret = credentials::parse(&record["secret"])?;
    let before = roundtrip(&secret, endpoints::CHECKIN_STATUS, device)?;
    // 缺enable不是已确认没活动，缺签到状态不是已确认未签到。
    let enabled = before["enable"]
        .as_bool()
        .ok_or_else(|| Error::http(502, "Trae 签到状态缺少活动开关"))?;
    if !enabled {
        return Ok(report(
            "inactive",
            "Trae 当前未开放每日签到活动",
            None,
            None,
        ));
    }
    if done(&before) {
        return Ok(report("done", "Trae 今日已经签到", credit(&before), None));
    }
    if before["checked_in"].as_bool().is_none() && before["did_checked_in"].as_bool().is_none() {
        return Err(Error::http(502, "Trae 签到状态不完整，未尝试领取"));
    }
    account::unchanged(&record["secret"])?;
    let claim = roundtrip(&secret, endpoints::CHECKIN_CLAIM, device)?;
    let after = match roundtrip(&secret, endpoints::CHECKIN_STATUS, device) {
        Ok(after) => after,
        Err(_) => {
            return Ok(report(
                "failed",
                "Trae 领取后回查未完成，不能确认奖励到账；本日暂停自动重试",
                None,
                Some(tomorrow(util::now()?)),
            ))
        }
    };
    account::unchanged(&record["secret"])?;
    if !done(&after) {
        return Ok(report(
            "failed",
            "Trae 领取返回成功但同设备回查未确认，本日暂停自动重试",
            None,
            Some(tomorrow(util::now()?)),
        ));
    }
    let amount = credit(&claim).or_else(|| credit(&after));
    Ok(report(
        "claimed",
        if amount.is_some() {
            "Trae 每日奖励已由回查确认"
        } else {
            "Trae 签到已确认，上游未给出奖励数额"
        },
        amount,
        None,
    ))
}

pub fn claim(params: &Value) -> Result<Value> {
    let _lock = AccountLock::acquire()?;
    let mut record = refresh::locked(account::current()?, false)?;
    let secret = credentials::parse(&record["secret"])?;
    let now = util::now()?;
    let old = sdk::state::get(Scope::Account, KEY, 1)?;
    if !old.compatible {
        return Err(Error::http(409, "Trae 签到状态版本不兼容"));
    }
    if params["manual"] != true
        && old.value["binding"] == account_binding(&secret)
        && old.value["retryAt"].as_i64().is_some_and(|at| at > now)
    {
        if old.value["completed"] == true {
            return Ok(report(
                "done",
                "Trae 今日已有确认过的签到记录",
                None,
                old.value["retryAt"].as_i64(),
            ));
        }
        return Ok(report(
            "failed",
            "Trae 本日签到已暂停自动重试，可手动重新发起",
            None,
            old.value["retryAt"].as_i64(),
        ));
    }
    // 先登记本轮预约，异常退出不能让调度器不断重复claim；人工再试仍只发一轮。
    let binding = account_binding(&secret);
    let until = tomorrow(now);
    if !sdk::state::compare_exchange(
        Scope::Account,
        KEY,
        1,
        old.revision,
        json!({ "binding": binding, "retryAt": until }),
        0,
    )? {
        return Err(Error::http(409, "Trae 另一轮签到正在处理"));
    }
    let reservation = sdk::state::get(Scope::Account, KEY, 1)?;
    let mut result = Err(Error::http(502, "Trae 签到未完成"));
    for attempt in 0..2 {
        let device = util::digits(16)?;
        result = run(&record, &device);
        let retry = result
            .as_ref()
            .err()
            .is_some_and(|e| e.code.as_deref() == Some("trae_session_expired"));
        if !retry
            || attempt == 1
            || util::text(&credentials::parse(&record["secret"])?, &["refreshToken"]).is_empty()
        {
            break;
        }
        record = refresh::locked(account::current()?, true)?;
    }
    let mut report = match result {
        Ok(report) => report,
        Err(error) if error.code.as_deref() == Some("trae_session_expired") => return Err(error),
        Err(error) => {
            let mut report = report("failed", &error.message, None, Some(until));
            if let Some(code) = error.upstream_code {
                report["details"] = json!({ "code": code });
            }
            report
        }
    };
    if matches!(
        report["outcome"].as_str(),
        Some("claimed" | "done" | "inactive")
    ) {
        report["retryAt"] = json!(until);
    }
    account::unchanged(&record["secret"])?;
    let completed = matches!(report["outcome"].as_str(), Some("claimed" | "done"));
    let binding = account_binding(&credentials::parse(&record["secret"])?);
    if !sdk::state::compare_exchange(
        Scope::Account,
        KEY,
        1,
        reservation.revision,
        json!({ "binding": binding, "retryAt": until, "completed": completed }),
        0,
    )? {
        return Err(Error::http(409, "Trae 签到记录已变化，未覆盖现有结果"));
    }
    Ok(report)
}
