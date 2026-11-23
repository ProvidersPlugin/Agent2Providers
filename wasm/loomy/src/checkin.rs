//! 每日首次登录刷新是明确动作；不在查询时提交、不把重复结果算作新增积分。
use super::{account, balance, client, errors};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    AccountLock, Error, Result, Value,
};

const KEY: &str = "daily-first-login";

fn persist(revision: u64, value: Value) -> Result<()> {
    if !state::compare_exchange(Scope::Account, KEY, 1, revision, value, 0)? {
        return Err(Error::http(409, "Loomy 每日积分记录已变化，未覆盖其它操作"));
    }
    Ok(())
}

pub fn claim() -> Result<Value> {
    let before_lock = account::current()?;
    let _lock = AccountLock::acquire()?;
    account::ensure_current(&before_lock)?;
    let record = account::current()?;
    let session = account::token(&record["secret"])?;
    let now = sdk::now();
    let day = (now + 8 * 3_600_000).div_euclid(86_400_000);
    let saved = state::get(Scope::Account, KEY, 1)?;
    if !saved.compatible {
        return Err(Error::http(409, "Loomy 每日积分记录格式已变化"));
    }
    if saved.value["day"] == day && saved.value["phase"] == "done" {
        return Ok(
            json!({ "outcome": "done", "credit": null, "streak": null, "message": "今日首次登录积分已确认刷新" }),
        );
    }
    let before = balance::daily(&balance::read(&record)?);
    if saved.value["day"] == day
        && matches!(saved.value["phase"].as_str(), Some("pending" | "uncertain"))
    {
        let old = saved.value["before"].as_f64();
        if before
            .zip(old)
            .is_some_and(|(after, before)| after > before)
        {
            persist(saved.revision, json!({ "day": day, "phase": "done" }))?;
            return Ok(
                json!({ "outcome": "done", "credit": null, "message": "已只读确认每日积分变化，未重复提交" }),
            );
        }
        return Ok(json!({ "outcome": "failed", "credit": null,
            "message": "此前每日积分刷新结果尚未确认，已查询余额但未重复提交，请稍后核对" }));
    }
    account::ensure_current(&record)?;
    persist(
        saved.revision,
        json!({ "day": day, "phase": "pending", "before": before }),
    )?;
    let reserved = state::get(Scope::Account, KEY, 1)?;
    let reply = client::token_request(
        "POST",
        "/api/v1/points/first-login",
        &session,
        Some(&json!({})),
        "每日积分刷新",
    );
    account::ensure_current(&record)?;
    let payload = match reply {
        Ok(payload) => payload,
        Err(error) => {
            let phase = if matches!(error.status, Some(400 | 401 | 403 | 429)) {
                "failed"
            } else {
                "uncertain"
            };
            persist(
                reserved.revision,
                json!({ "day": day, "phase": phase, "before": before }),
            )?;
            return Err(error);
        }
    };
    if let Err(error) = errors::ensure_success(&payload, "每日积分刷新", &session) {
        let phase = if errors::business_code(&payload).is_empty() {
            "uncertain"
        } else {
            "failed"
        };
        persist(
            reserved.revision,
            json!({ "day": day, "phase": phase, "before": before }),
        )?;
        return Err(error);
    }
    let data = balance::data(&payload);
    let after = balance::daily(&payload).or_else(|| {
        balance::read(&record)
            .ok()
            .and_then(|value| balance::daily(&value))
    });
    let already = data["alreadyProcessed"] == true;
    let newly = !already
        && (data["alreadyProcessed"] == false
            || before
                .zip(after)
                .is_some_and(|(before, after)| after > before));
    let credit = (!already)
        .then(|| {
            before
                .zip(after)
                .map(|(before, after)| (after - before).max(0.0))
        })
        .flatten()
        .filter(|credit| *credit > 0.0);
    account::ensure_current(&record)?;
    persist(reserved.revision, json!({ "day": day, "phase": "done" }))?;
    Ok(
        json!({ "outcome": if newly { "claimed" } else { "done" }, "credit": credit.map(|value| value.to_string()),
        "streak": null, "message": after.map(|value| format!("每日赠送积分已刷新（当前 {value}）"))
            .unwrap_or_else(|| "每日首次登录积分刷新已完成".into()) }),
    )
}
