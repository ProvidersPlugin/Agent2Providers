//! 每日登录初始化只在本账号当天第一次真实请求时提交；未知结果只回查积分，不盲目重发。
//! 两池正增量才报 claimed，零增量不假报 done；每日预约保存在宿主账号范围，锁内 CAS。

use super::{account, client, endpoints, usage};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{Scope, Snapshot},
    AccountLock, Error, Result, Value,
};
use sha2::{Digest, Sha256};

const KEY: &str = "daily-login-v1";

fn report(outcome: &str, message: &str, credit: Option<String>) -> Value {
    json!({ "outcome": outcome, "message": message, "credit": credit, "streak": null })
}

fn save(previous: &Snapshot, value: Value) -> Result<Snapshot> {
    if !sdk::state::compare_exchange(Scope::Account, KEY, 1, previous.revision, value, 0)? {
        return Err(Error::http(409, "AStudio 每日登录状态已变化，未重复提交"));
    }
    let current = sdk::state::get(Scope::Account, KEY, 1)?;
    if !current.compatible {
        return Err(Error::http(409, "AStudio 每日登录状态版本不兼容"));
    }
    Ok(current)
}

fn observe(record: &Value, previous: &Snapshot) -> Result<Value> {
    let after = usage::balance(&record["secret"])?;
    account::ensure_current(record)?;
    let delta = |before: &Value, after: Option<f64>| {
        after
            .zip(account::number(before))
            .map(|(after, before)| after - before)
            .filter(|value| value.is_finite())
    };
    let deltas = [
        (
            "通用积分",
            delta(&previous.value["before"]["points"], after.points),
        ),
        (
            "星火积分",
            delta(&previous.value["before"]["spark"], after.spark),
        ),
    ];
    let rewards: Vec<String> = deltas
        .iter()
        .filter_map(|(label, delta)| {
            delta
                .filter(|delta| *delta > 0.0)
                .map(|delta| format!("{label} +{delta}"))
        })
        .collect();
    if rewards.is_empty() {
        return Ok(report(
            "failed",
            "每日登录结果尚未确认；后续只回查积分，不重复提交初始化",
            None,
        ));
    }
    let credit = rewards.join("，");
    let mut value = previous.value.clone();
    value["phase"] = json!("observed");
    value["credit"] = json!(credit);
    save(previous, value)?;
    Ok(report(
        "claimed",
        &format!("每日登录奖励已观测到账：{credit}"),
        Some(credit.clone()),
    ))
}

fn run() -> Result<Value> {
    let _lock = AccountLock::acquire()?;
    let record = account::current()?;
    let secret = &record["secret"];
    if !account::has_session(secret) {
        return Ok(report(
            "ineligible",
            "该账号只有模型凭据，无网页会话，无法执行每日登录签到",
            None,
        ));
    }
    let now = sdk::now();
    if now <= 0 {
        return Err(Error::http(502, "AStudio 每日登录无法取得当前时间"));
    }
    let day = now.saturating_add(8 * 3_600_000) / 86_400_000;
    let fingerprint = format!("{:x}", Sha256::digest(account::cookie(secret)?.as_bytes()));
    let previous = sdk::state::get(Scope::Account, KEY, 1)?;
    if !previous.compatible {
        return Err(Error::http(409, "AStudio 每日登录状态版本不兼容"));
    }
    if previous.value["day"].as_i64() == Some(day) {
        if previous.value["fingerprint"] != fingerprint {
            return Ok(report(
                "failed",
                "今日网页会话已更换，原初始化结果需先确认；未重复提交",
                None,
            ));
        }
        return match previous.value["phase"].as_str() {
            Some("observed") => Ok(report("done", "今日每日登录奖励已经观测到账", None)),
            Some("pending") => observe(&record, &previous),
            _ => Err(Error::http(
                409,
                "AStudio 今日每日登录状态不完整，未重复提交",
            )),
        };
    }
    let config = client::studio("GET", endpoints::PLAN, Some(secret))?.data(true, "session")?;
    if config["dailyLoginEnabled"] == false {
        return Ok(report("inactive", "当前没有每日登录积分活动", None));
    }
    let before = usage::balance(secret)?;
    account::ensure_current(&record)?;
    let reserved = save(
        &previous,
        json!({ "day": day, "phase": "pending", "fingerprint": fingerprint,
        "before": { "points": before.points, "spark": before.spark } }),
    )?;
    account::ensure_current(&record)?;
    client::init(secret)?;
    sdk::sleep(2500)?;
    observe(&record, &reserved)
}

pub fn claim() -> Result<Value> {
    match run() {
        Ok(report) => Ok(report),
        Err(error)
            if matches!(
                error.code.as_deref(),
                Some("astudio_session_expired" | "astudio_model_expired")
            ) =>
        {
            Err(error)
        }
        Err(error) => Ok(report("failed", &error.message, None)),
    }
}
