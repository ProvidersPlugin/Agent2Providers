//! 每日登录签到：检查活动配置与套餐资格 → 读余额 → init-app 触发当日登录 →
//! 查 client-popups/pending 并提交 DAILY_REWARD_DIALOG 领取 → 回查两池余额确认到账。
//! 兼容旧版遗留的同日 pending 状态；每日状态保存在宿主账号范围，锁内 CAS。

use super::{account, client, endpoints, usage};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{Scope, Snapshot},
    AccountLock, Error, Result, Value,
};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

const KEY: &str = "daily-login-v1";
const POPUP_RETRY_WAIT_MS: u64 = 1500;
const REWARD_WAIT_MS: u64 = 2500;

struct RewardPopup {
    popup_id: i64,
    instance_key: String,
}

fn report(outcome: &str, message: &str, credit: Option<String>) -> Value {
    json!({ "outcome": outcome, "message": message, "credit": credit, "streak": null, "retryAt": null })
}

fn format_delta(value: f64) -> String {
    if value.fract().abs() < 0.005 {
        format!("{}", value.round() as i64)
    } else {
        format!("{value:.2}")
    }
}

fn plan_zero_reason(config: &Value, membership: &Value) -> Option<String> {
    let code = membership["planCode"].as_str().map(str::trim).filter(|v| !v.is_empty())?;
    let plan = config["plans"]
        .as_array()?
        .iter()
        .find(|item| item["planCode"].as_str().is_some_and(|candidate| candidate.eq_ignore_ascii_case(code)))?;
    let points = account::number(&plan["dailyLoginPoints"])?;
    if points > 0.0 {
        return None;
    }
    let label = account::text(plan, &["planName"]);
    let display = if label.is_empty() {
        code.to_string()
    } else {
        label
    };
    Some(format!(
        "当前套餐（{display}）每日登录积分为 0，没有可领取的每日登录奖励"
    ))
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

fn positive_rewards(before: &usage::Balance, after: &usage::Balance) -> Vec<String> {
    let delta = |before: Option<f64>, after: Option<f64>| {
        before
            .zip(after)
            .map(|(before, after)| after - before)
            .filter(|value| value.is_finite() && *value > 0.0)
    };
    [
        ("通用积分", delta(before.points, after.points)),
        ("星火积分", delta(before.spark, after.spark)),
    ]
    .into_iter()
    .filter_map(|(label, value)| value.map(|value| format!("{label} +{}", format_delta(value))))
    .collect()
}

fn balance_from_state(value: &Value) -> usage::Balance {
    usage::Balance {
        points: account::number(&value["before"]["points"]),
        spark: account::number(&value["before"]["spark"]),
    }
}

fn is_daily_reward_popup(item: &Value) -> bool {
    let component = item["componentType"]
        .as_str()
        .or_else(|| item["type"].as_str())
        .map(str::trim)
        .unwrap_or("");
    if component.eq_ignore_ascii_case("DAILY_REWARD_DIALOG") {
        return true;
    }
    let code = item["popupCode"]
        .as_str()
        .map(str::trim)
        .unwrap_or("")
        .to_ascii_uppercase();
    code.contains("DAILY") && code.contains("REWARD")
}

fn popup_id(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|v| i64::try_from(v).ok()))
        .or_else(|| value.as_str().and_then(|v| v.trim().parse::<i64>().ok()))
}

fn fetch_daily_popups(secret: &Value) -> Result<Vec<RewardPopup>> {
    let data = client::studio("GET", endpoints::POPUPS_PENDING, Some(secret))?.confirm("session")?;
    let list = if data.is_null() {
        return Ok(Vec::new());
    } else if let Some(items) = data.as_array() {
        items
    } else if let Some(items) = data["popups"].as_array() {
        items
    } else {
        return Err(Error::http(502, "AStudio 待领取奖励列表格式异常"));
    };

    let mut matched = 0usize;
    let mut rewards = Vec::new();
    let mut seen = HashSet::new();
    for item in list {
        if !item.is_object() || !is_daily_reward_popup(item) {
            continue;
        }
        matched += 1;
        let Some(id) = popup_id(&item["popupId"]) else {
            continue;
        };
        let instance_key = account::text(item, &["instanceKey"]);
        if instance_key.is_empty() {
            continue;
        }
        if seen.insert((id, instance_key.clone())) {
            rewards.push(RewardPopup {
                popup_id: id,
                instance_key,
            });
        }
    }
    if matched > 0 && rewards.is_empty() {
        return Err(Error::http(502, "AStudio 待领取每日奖励缺少必要标识"));
    }
    Ok(rewards)
}

fn claim_daily_popups(record: &Value) -> (usize, Option<Error>) {
    let secret = &record["secret"];
    let popups = match fetch_daily_popups(secret) {
        Ok(popups) => popups,
        Err(error) => return (0, Some(error)),
    };
    let mut claimed = 0usize;
    for popup in popups {
        if let Err(error) = account::ensure_current(record) {
            return (claimed, Some(error));
        }
        let body = json!({
            "popupId": popup.popup_id,
            "instanceKey": popup.instance_key,
            "completed": true,
        });
        if let Err(error) = client::studio_json("POST", endpoints::POPUPS_COMPLETE, Some(secret), &body)
            .and_then(|reply| reply.confirm("session"))
        {
            return (claimed, Some(error));
        }
        claimed += 1;
    }
    (claimed, None)
}

fn state_payload(day: i64, phase: &str, fingerprint: &str, before: &usage::Balance, credit: Option<&str>) -> Value {
    let mut value = json!({
        "day": day,
        "phase": phase,
        "fingerprint": fingerprint,
        "before": { "points": before.points, "spark": before.spark },
    });
    if let Some(credit) = credit {
        value["credit"] = json!(credit);
    }
    value
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
    let same_day = previous.value["day"].as_i64() == Some(day);
    let same_session = same_day && previous.value["fingerprint"] == fingerprint;
    let prev_phase = if same_day {
        previous.value["phase"].as_str()
    } else {
        None
    };

    if prev_phase == Some("observed") {
        return Ok(report("done", "今日每日登录奖励已到账", None));
    }

    let config = client::studio("GET", endpoints::PLAN, Some(secret))?.data(true, "session")?;
    if config["dailyLoginEnabled"] == false {
        return Ok(report("inactive", "当前没有每日登录积分活动", None));
    }
    match client::studio("GET", endpoints::MEMBERSHIP, Some(secret))
        .and_then(|reply| reply.data(true, "session"))
    {
        Ok(membership) => {
            if let Some(reason) = plan_zero_reason(&config, &membership) {
                return Ok(report("ineligible", &reason, None));
            }
        }
        Err(error) if error.code.as_deref() == Some("astudio_session_expired") => return Err(error),
        Err(_) => {}
    }

    let before = usage::balance(secret)?;
    account::ensure_current(&record)?;

    // 同日此前已初始化或已确认领取但当时余额尚未刷新：先核对此时余额是否已到账。
    if same_session && matches!(prev_phase, Some("pending" | "claimed")) {
        let prev_before = balance_from_state(&previous.value);
        let delayed = positive_rewards(&prev_before, &before);
        if !delayed.is_empty() {
            let credit = delayed.join("，");
            save(
                &previous,
                state_payload(day, "observed", &fingerprint, &prev_before, Some(&credit)),
            )?;
            return Ok(report(
                "claimed",
                &format!("每日登录奖励已观测到账：{credit}"),
                Some(credit),
            ));
        }
    }

    let already_claimed_today = same_session && prev_phase == Some("claimed");
    let snapshot = save(
        &previous,
        state_payload(
            day,
            if already_claimed_today { "claimed" } else { "pending" },
            &fingerprint,
            &before,
            None,
        ),
    )?;
    account::ensure_current(&record)?;

    if !already_claimed_today {
        client::init(secret)?;
    }

    let (mut claimed_count, mut claim_error) = claim_daily_popups(&record);
    if let Some(error) = claim_error.as_ref() {
        if matches!(
            error.code.as_deref(),
            Some("astudio_session_expired" | "astudio_model_expired")
        ) {
            return Err(claim_error.unwrap());
        }
    }

    // 刚执行完初始化时，上游推广位可能存在短暂异步延迟；为空时稍等复查一次再下结论。
    if claimed_count == 0 && claim_error.is_none() && !already_claimed_today {
        sdk::sleep(POPUP_RETRY_WAIT_MS)?;
        account::ensure_current(&record)?;
        let (retry_claimed, retry_error) = claim_daily_popups(&record);
        if let Some(error) = retry_error.as_ref() {
            if matches!(
                error.code.as_deref(),
                Some("astudio_session_expired" | "astudio_model_expired")
            ) {
                return Err(retry_error.unwrap());
            }
        }
        claimed_count = retry_claimed;
        claim_error = retry_error;
    }

    if claimed_count > 0 {
        sdk::sleep(REWARD_WAIT_MS)?;
    }
    let after = usage::balance(secret)?;
    account::ensure_current(&record)?;

    let rewards = positive_rewards(&before, &after);
    if !rewards.is_empty() {
        let credit = rewards.join("，");
        save(
            &snapshot,
            state_payload(day, "observed", &fingerprint, &before, Some(&credit)),
        )?;
        return Ok(report(
            "claimed",
            &format!("每日登录奖励已到账：{credit}"),
            Some(credit),
        ));
    }

    if claimed_count > 0 {
        let _ = save(
            &snapshot,
            state_payload(day, "claimed", &fingerprint, &before, None),
        )?;
        return Ok(report(
            "claimed",
            &format!(
                "已领取今日每日登录奖励（服务端确认 {claimed_count} 项），积分读数暂未更新，稍后刷新余额可见"
            ),
            None,
        ));
    }

    if let Some(error) = claim_error {
        return Ok(report(
            "failed",
            &format!("每日登录已执行，但领取每日登录奖励失败：{}", error.message),
            None,
        ));
    }

    if already_claimed_today {
        return Ok(report(
            "done",
            "今日每日登录奖励已领取，当前没有待领取项",
            None,
        ));
    }

    Ok(report(
        "done",
        "今日每日登录已完成，当前没有待领取的每日登录奖励（可能今日已领过）",
        None,
    ))
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
