//! 登录信息触发魔粒发放，但成功响应只有身份。签到必须由当日 daily_active 流水确认。
//! 在账号锁内先重读原行；已提交但未知结果只读核对，不因断流/重启再次触发发放。

use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

use super::{
    account, balance,
    client::Client,
    clock,
    transactions::{self, Transactions},
    Region,
};

pub const LOGIN_INFO_PATH: &str = "/api/v1/users/login/info";
const JOURNAL: &str = "modelscopeDailyActive";

fn save(source: &account::Snapshot, value: &Value) -> Result<()> {
    source.check()?;
    sdk::host("ledger.put", json!({ "kind": JOURNAL, "value": value }))?;
    source.check()
}

pub fn claim(region: Region) -> Result<Value> {
    let source = account::Snapshot::read(region)?;
    let _lock = AccountLock::acquire()?;
    source.check()?;
    let api = Client::for_snapshot(region, source.clone())?;
    let day = clock::today()?;
    let binding = json!([
        source.record["provider"],
        source.record["id"],
        source.record["identity"],
        account::user_id(&source.record["secret"])
    ]);
    let previous = sdk::host("ledger.get", json!({ "kind": JOURNAL }))?;
    if !previous.is_null() && !previous.is_object() {
        return Err(Error::http(409, "ModelScope 签到台账格式异常，未触发发放"));
    }
    let before = transactions::read(&api, &day)?;
    if before.signed_on(&day) {
        return Ok(confirmed("done", &before, None, &day));
    }
    if previous["day"] == day && previous["binding"] == binding && previous["confirmed"] == true {
        return Ok(unknown(
            "本日已确认发放，但最新流水未包含该记录；只读核对，不重复触发".to_string(),
        ));
    }
    if !before.complete {
        return Ok(failed(
            "发放流水尚未读全，未触发签到，请稍后只读核对".to_string(),
        ));
    }
    let already_submitted =
        previous["day"] == day && previous["binding"] == binding && previous["submitted"] == true;
    let mut journal = if already_submitted {
        previous
    } else {
        json!({ "day": day, "binding": binding, "submitted": false })
    };
    let mut trigger_error = None;
    if !already_submitted {
        if clock::today()? != day {
            return Ok(unknown(
                "查询跨过北京时间零点，未用前日状态触发新一日发放，请重新核对".to_string(),
            ));
        }
        journal["submitted"] = json!(true);
        journal["attemptAt"] = json!(sdk::now());
        save(&source, &journal)?;
        if let Err(error) = api.site_json(LOGIN_INFO_PATH, "每日签到") {
            if matches!(error.code.as_deref(), Some("account_changed" | "forbidden")) {
                return Err(error);
            }
            trigger_error = Some(error.message);
        }
    }
    let mut observed_day = day.clone();
    for attempt in 0..3 {
        if attempt > 0 {
            sdk::sleep(2000)?;
        }
        source.check()?;
        observed_day = clock::today()?;
        if observed_day != day {
            return Ok(unknown(
                "提交或核对跨过北京时间零点，原日发放尚未确认；未将另一日的流水计为本次签到，也未重复提交".to_string(),
            ));
        }
        let after = match transactions::read(&api, &observed_day) {
            Ok(after) => after,
            Err(error)
                if matches!(error.code.as_deref(), Some("account_changed" | "forbidden")) =>
            {
                return Err(error)
            }
            Err(error) => {
                return Ok(unknown(format!(
                    "已触发或提交结果未知，发放流水读取失败：{}；未重复签到",
                    error.message
                )))
            }
        };
        if after.signed_on(&observed_day) {
            let gained = if !already_submitted && observed_day == day {
                after.daily_increase(&before, &day)
            } else {
                None
            };
            journal["confirmed"] = json!(true);
            save(&source, &journal)?;
            let outcome = if gained.is_some() { "claimed" } else { "done" };
            let mut report = confirmed(outcome, &after, gained, &observed_day);
            if let Ok(Some(balance)) = balance::current_balance(&api) {
                let message = report["message"].as_str().unwrap_or("");
                report["message"] = json!(format!(
                    "{message}（当前 {} 魔粒）",
                    transactions::amount_text(balance)
                ));
            }
            source.check()?;
            return Ok(report);
        }
    }
    let detail = trigger_error
        .map(|error| format!("（{error}）"))
        .unwrap_or_default();
    source.check()?;
    Ok(unknown(format!(
        "{observed_day} 的签到发放尚未确认{detail}；后续仅核对流水，不重复触发"
    )))
}

fn confirmed(outcome: &str, ledger: &Transactions, gained: Option<f64>, day: &str) -> Value {
    let message = match gained {
        Some(amount) => format!("已确认领取 {} 魔粒", transactions::amount_text(amount)),
        None => match ledger
            .complete
            .then(|| ledger.total_on(day, true))
            .flatten()
        {
            Some(amount) => format!(
                "今日签到已确认，日常发放共 {} 魔粒",
                transactions::amount_text(amount)
            ),
            None => "今日签到已确认，发放金额未读全".to_string(),
        },
    };
    json!({ "outcome": outcome, "message": message,
        "credit": gained.map(|amount| format!("+{} 魔粒", transactions::amount_text(amount))), "streak": null })
}

fn failed(message: String) -> Value {
    json!({ "outcome": "failed", "message": message, "credit": null, "streak": null })
}

fn unknown(message: String) -> Value {
    json!({ "outcome": "inactive", "message": message, "credit": null, "streak": null,
        "retryAt": sdk::now().saturating_add(600_000) })
}
