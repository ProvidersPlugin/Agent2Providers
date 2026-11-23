use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope, Snapshot},
    Error, Result, Value,
};

use crate::{account, endpoints, util};

const KEY: &str = "dumateDailyBonus";

pub struct Status {
    pub issued: bool,
    pub day: String,
    pub days: Vec<String>,
    pub total_points: Option<f64>,
    pub total_times: Option<f64>,
    pub month_points: Option<f64>,
}

impl Status {
    pub fn query(current: &account::Snapshot) -> Result<Self> {
        let day = util::date(util::now()?)?;
        let value = current.web("GET", endpoints::BONUS_INFO, None)?;
        if util::date(util::now()?)? != day {
            return Err(Error::http(
                409,
                "百度搭子签到查询跨过了零点，未提交签到，请刷新后重试",
            ));
        }
        let issued = value
            .get("hasIssued")
            .or_else(|| value.get("has_issued"))
            .and_then(Value::as_bool)
            .ok_or_else(|| Error::http(502, "百度搭子未返回明确的今日签到状态，未提交签到"))?;
        let days = value
            .get("signInDays")
            .or_else(|| value.get("sign_in_days"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(366)
            .filter_map(Value::as_str)
            .filter(|date| chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok())
            .map(str::to_string)
            .collect();
        Ok(Self {
            issued,
            day,
            days,
            total_points: util::number(value.get("totalPoints").unwrap_or(&value["total_points"])),
            total_times: util::number(value.get("totalTimes").unwrap_or(&value["total_times"])),
            month_points: util::number(value.get("monthPoints").unwrap_or(&value["month_points"])),
        })
    }
}

fn get() -> Result<Snapshot> {
    let value = state::get(Scope::Account, KEY, 1)?;
    if !value.compatible {
        return Err(Error::http(409, "百度搭子签到记录版本不兼容，未提交签到"));
    }
    Ok(value)
}

fn save(current: &account::Snapshot, value: &Snapshot, state_name: &str, day: &str) -> Result<()> {
    current.unchanged()?;
    if !value.compatible
        || !state::compare_exchange(
            Scope::Account,
            KEY,
            1,
            value.revision,
            json!({ "day": day, "identity": current.identity, "state": state_name, "at": util::now()? }),
            0,
        )?
    {
        return Err(Error::http(409, "百度搭子签到记录已变化，未重复提交签到"));
    }
    Ok(())
}

pub fn saved(current: &account::Snapshot, status: &Status) -> Result<String> {
    let saved = get()?;
    Ok(
        if saved.value["day"] == status.day && saved.value["identity"] == current.identity {
            saved.value["state"].as_str().unwrap_or("").to_string()
        } else {
            String::new()
        },
    )
}

fn report(outcome: &str, message: &str, status: Option<&Status>) -> Value {
    json!({ "outcome": outcome, "message": message, "credit": null, "streak": null,
        "totalPoints": status.and_then(|status| status.total_points),
        "totalTimes": status.and_then(|status| status.total_times),
        "monthPoints": status.and_then(|status| status.month_points),
        "signInDays": status.map(|status| &status.days) })
}

fn run(expected_day: Option<&str>) -> Result<Value> {
    let _lock = sdk::AccountLock::acquire()?;
    let current = account::current()?;
    current.verify()?;
    let before = Status::query(&current)?;
    if expected_day.is_some_and(|day| day != before.day) {
        return Ok(report(
            "inactive",
            "百度搭子签到日期已变化，请刷新后重新选择",
            Some(&before),
        ));
    }
    let previous = get()?;
    let saved =
        if previous.value["day"] == before.day && previous.value["identity"] == current.identity {
            previous.value["state"].as_str().unwrap_or("")
        } else {
            ""
        };
    if before.issued {
        save(&current, &previous, "claimed", &before.day)?;
        return Ok(report(
            "done",
            "官方确认今日登录积分已发放，本次没有重复领取",
            Some(&before),
        ));
    }
    if matches!(saved, "confirming" | "claimed") {
        return Ok(report(
            "inactive",
            "百度搭子上次签到结果待确认，只查询状态，不重复提交",
            Some(&before),
        ));
    }
    save(&current, &previous, "confirming", &before.day)?;
    if util::date(util::now()?)? != before.day {
        return Ok(report(
            "inactive",
            "签到日期刚刚变化，未提交签到，请刷新后重试",
            Some(&before),
        ));
    }
    let submitted = current.web("POST", endpoints::BONUS, None);
    if matches!(submitted, Ok(Value::Bool(true))) {
        save(&current, &get()?, "claimed", &before.day)?;
        return Ok(report(
            "claimed",
            "官方已确认今日登录积分发放，本次不会因后续查询失败再次领取",
            Some(&before),
        ));
    }
    let after = Status::query(&current);
    if let Ok(after) = &after {
        if after.day == before.day && after.issued {
            save(&current, &get()?, "claimed", &before.day)?;
            return Ok(report(
                "claimed",
                "官方已确认今日登录积分发放；奖励金额不按余额差估算",
                Some(after),
            ));
        }
    }
    if let Err(error) = submitted {
        if matches!(
            error.code.as_deref(),
            Some(
                "dumate_upstream"
                    | "dumate_rate_limited"
                    | "dumate_quota_exhausted"
                    | "dumate_content_blocked"
            )
        ) && error
            .status
            .is_some_and(|status| (400..500).contains(&status))
        {
            save(&current, &get()?, "rejected", &before.day)?;
            return Ok(report("failed", &error.message, after.as_ref().ok()));
        }
    }
    Ok(report(
        "inactive",
        "签到已提交但结果尚未确认，稍后可再次查询；不会自动重复领取",
        after.as_ref().ok(),
    ))
}

pub fn claim_for(expected_day: Option<&str>) -> Result<Value> {
    match run(expected_day) {
        Ok(result) => Ok(result),
        Err(error) => Ok(report("failed", &error.message, None)),
    }
}

pub fn info() -> Result<Value> {
    let current = account::current()?;
    let status = Status::query(&current)?;
    Ok(
        json!({ "day": status.day, "hasIssued": status.issued, "signInDays": status.days,
        "totalPoints": status.total_points, "totalTimes": status.total_times,
        "monthPoints": status.month_points, "savedState": saved(&current, &status)? }),
    )
}
