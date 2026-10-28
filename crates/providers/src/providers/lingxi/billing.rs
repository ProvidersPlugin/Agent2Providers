//! 智点余额与每日签到。
//!
//! 余额：`GET /api/public/v1/credits/balance` → `data.total_balance` 与逐份
//! `bonus_credits[]`（面额 / 已用 / 剩余 / 到期）。签到：任务中心
//! `GET /api/public/v1/tasks` 里 `daily_check_in`，领取走
//! `POST .../daily_check_in/claim?date=YYYY-MM-DD`。
//!
//! ── 为什么签到要「领前领后读两次余额」────────────────────────
//! 上游对重复领取是**幂等假成功**：返回 ok 但余额不动。只看返回码会把
//! 「今天已经签过」记成「刚签到成功」。所以判据是余额增量，而不是响应体。

use agent2api_provider_protocol as proto;
use serde_json::{json, Value};

use agent2api_provider_sdk::PluginCtx;

use super::account;
use super::constants;
use super::http;

/// 查余额，归一成网关的 usage 形状。
pub async fn usage(
    ctx: &PluginCtx,
    account_id: &str,
    proxy: Option<&proto::WireProxy>,
) -> Result<proto::UsageReport, String> {
    let sid = account::sid_for(ctx, account_id).await?;
    let client = http::build_client(proxy, &ctx.env.proxy);
    let payload = http::get_json(&client, &sid, constants::BALANCE_PATH).await?;
    let data = http::data_of(&payload);
    Ok(shape_usage(&data))
}

/// 纯函数：上游 balance 的 `data` → 网关 usage。
pub fn shape_usage(data: &Value) -> Value {
    let number = |key: &str| data.get(key).and_then(Value::as_f64).filter(|value| value.is_finite()).unwrap_or(0.0);
    let total = number("total_balance");
    let bonus = number("total_bonus");
    let purchased = number("total_purchased");

    let mut credits = Vec::new();
    for (kind, key) in [("赠送智点", "bonus_credits"), ("购买智点", "purchased_credits")] {
        if let Some(items) = data.get(key).and_then(Value::as_array) {
            for (index, item) in items.iter().enumerate() {
                let remain = item.get("balance").and_then(Value::as_f64).unwrap_or(0.0);
                let value = item.get("value").and_then(Value::as_f64).unwrap_or(0.0);
                let used = item.get("consumed").and_then(Value::as_f64).unwrap_or(0.0);
                // 网关的「积分明细」按毫秒时刻排序（`credit_expiry`）
                let expire_at = item
                    .get("expire_time")
                    .and_then(Value::as_str)
                    .and_then(|text| chrono::DateTime::parse_from_rfc3339(text.trim()).ok())
                    .map(|value| value.timestamp_millis())
                    .unwrap_or(0);
                credits.push(json!({
                    "name": format!("{kind} #{}", index + 1),
                    "remain": remain,
                    "total": value,
                    "used": used,
                    "unit": "智点",
                    "expireAt": expire_at,
                }));
            }
        }
    }

    json!({
        "available": total,
        "unit": "智点",
        "wallets": [
            { "type": "bonus", "displayName": "赠送智点", "balance": bonus, "remaining": bonus },
            { "type": "purchased", "displayName": "购买智点", "balance": purchased, "remaining": purchased },
        ],
        "credits": credits,
        "subscription": data.get("active_subscription").cloned().unwrap_or(Value::Null),
        "raw": data,
    })
}

/// 每日签到。返回 `CheckinReport`（判据是余额增量）。
pub async fn checkin(
    ctx: &PluginCtx,
    account_id: &str,
    proxy: Option<&proto::WireProxy>,
) -> Result<proto::CheckinReport, String> {
    let sid = account::sid_for(ctx, account_id).await?;
    let client = http::build_client(proxy, &ctx.env.proxy);

    let tasks = http::get_json(&client, &sid, constants::TASKS_PATH).await?;
    let data = http::data_of(&tasks);
    let list = data.get("tasks").and_then(Value::as_array).cloned().unwrap_or_default();
    let Some(daily) = list
        .iter()
        .find(|task| task.get("task_key").and_then(Value::as_str) == Some("daily_check_in"))
    else {
        return Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Inactive,
            credit: None,
            streak: None,
            message: Some("上游任务中心里没有每日签到".to_string()),
        });
    };

    let status = daily.get("status").and_then(Value::as_str).unwrap_or("");
    let extra = daily.get("extra").cloned().unwrap_or(Value::Null);
    let streak = extra.get("consecutive_days").and_then(Value::as_u64).map(|value| value as u32);
    let mut outcome = daily_outcome(&client, &sid, daily, streak.clone()).await;
    let bonus = claim_one_time(&client, &sid, &list).await;
    if !bonus.credited.is_empty() {
        let keys: Vec<&str> = bonus.credited.iter().map(|(key, _)| key.as_str()).collect();
        let extra = format!("；一次性任务 {} 共 +{} 智点", keys.join("/"), trim_number(bonus.reward));
        outcome.credit = Some(match outcome.credit.take() {
            Some(credit) => format!("{credit}{extra}"),
            None => extra.trim_start_matches("；").to_string(),
        });
        if outcome.outcome != proto::CheckinOutcome::Claimed {
            outcome.outcome = proto::CheckinOutcome::Claimed;
        }
    }
    if let Some(warning) = bonus.warning {
        outcome.message = Some(match outcome.message.take() {
            Some(message) => format!("{message}（{warning}）"),
            None => warning,
        });
    }
    if status == "claimed" && outcome.message.is_none() {
        outcome.message = Some("今日已签到".to_string());
    }
    Ok(outcome)
}

/// 每日签到那一条的完整判定（领前读状态 → 领 → 领后回读余额）。
async fn daily_outcome(
    client: &reqwest::Client,
    sid: &str,
    daily: &Value,
    streak: Option<u32>,
) -> proto::CheckinReport {
    let status = daily.get("status").and_then(Value::as_str).unwrap_or("");
    if status == "claimed" {
        return proto::CheckinReport {
            outcome: proto::CheckinOutcome::Done,
            credit: None,
            streak,
            message: Some("今日已签到".to_string()),
        };
    }

    // 领取日期取任务里「今天可领」那一档的 date（+08 口径由上游给，别自己算）。
    let date = daily
        .get("extra")
        .and_then(|extra| extra.get("check_in_days"))
        .and_then(Value::as_array)
        .and_then(|days| {
            days.iter()
                .find(|day| day.get("status").and_then(Value::as_str) == Some("available"))
                .and_then(|day| day.get("date").and_then(Value::as_str))
        })
        .map(str::to_string)
        .unwrap_or_else(today_cst);

    let before = balance_total(client, sid).await.unwrap_or(0.0);
    let claim = http::post_json(
        client,
        sid,
        &format!("{}?date={}", constants::CLAIM_PATH, date),
        &json!({}),
        &[],
    )
    .await;
    if let Err(message) = claim {
        return proto::CheckinReport {
            outcome: proto::CheckinOutcome::Failed,
            credit: None,
            streak,
            message: Some(message),
        };
    }
    let after = balance_total(client, sid).await.unwrap_or(before);
    let delta = after - before;
    if delta > 0.0 {
        return proto::CheckinReport {
            outcome: proto::CheckinOutcome::Claimed,
            credit: Some(format!("+{} 智点", trim_number(delta))),
            streak: streak.map(|value| value + 1),
            message: None,
        };
    }
    // 返回 ok 但余额没动：上游幂等，今天已经领过
    proto::CheckinReport {
        outcome: proto::CheckinOutcome::Done,
        credit: None,
        streak,
        message: Some("今日已签到（上游幂等，余额未变化）".to_string()),
    }
}

/// 一次性任务的结果（只领 `status == "claimable"` 的）。
struct Bonus {
    /// 已确认到账的 `(task_key, 智点)`。
    credited: Vec<(String, i64)>,
    reward: f64,
    /// 回执有数额但余额没跟上（记账延迟 / 假成功）。
    warning: Option<String>,
}

/// 领掉所有**可领**的一次性任务，并用余额差确认到账。
///
/// ── 为什么只领 `claimable` ───────────────────────────────────
/// 实测（2026-10-16 一手）：`incomplete` 的条目 claim 会被上游拒
/// （400 `InvalidArgument`），`claimed` 的再领是幂等假成功。而
/// `incomplete → claimable` 只能由**真实动作**换来 —— 灵犀没有「上报完成」
/// 的口子。所以这里不替用户刷任务，只在上游说「可以领了」时把奖励领回来。
///
/// 单条失败只记账不拖垮每日签到（每条的错都吞进 warning / 日志）。
async fn claim_one_time(client: &reqwest::Client, sid: &str, tasks: &[Value]) -> Bonus {
    let wanted = super::onboarding::claimable_one_time(tasks);
    if wanted.is_empty() {
        return Bonus { credited: Vec::new(), reward: 0.0, warning: None };
    }

    let before = balance_total(client, sid).await.unwrap_or(-1.0);
    let mut receipts: Vec<(String, i64)> = Vec::new();
    for (key, expected) in wanted {
        let path = format!("{}/{}/claim", constants::TASKS_PATH, key);
        match http::post_json(client, sid, &path, &json!({}), &[]).await {
            Ok(payload) => {
                let got = http::data_of(&payload)
                    .get("reward_amount")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                if got > 0 {
                    receipts.push((key, got));
                } else {
                    crate::logging::log(
                        "[Lingxi]",
                        &format!("一次性任务 {key} 回执没有数额（预期 {expected}），按未到账处理"),
                    );
                }
            }
            Err(error) => {
                crate::logging::log("[Lingxi]", &format!("一次性任务 {key} 领取失败：{error}"));
            }
        }
    }
    if receipts.is_empty() {
        return Bonus { credited: Vec::new(), reward: 0.0, warning: None };
    }
    let expected: f64 = receipts.iter().map(|(_, reward)| *reward as f64).sum();
    let after = balance_total(client, sid).await.unwrap_or(before);
    if before >= 0.0 && after - before >= expected {
        return Bonus { credited: receipts, reward: expected, warning: None };
    }
    Bonus {
        credited: Vec::new(),
        reward: 0.0,
        warning: Some(format!(
            "一次性任务回执 +{} 但余额未同步（{}→{}），下次签到会重试",
            trim_number(expected),
            trim_number(before),
            trim_number(after)
        )),
    }
}

pub(super) async fn balance_total(client: &reqwest::Client, sid: &str) -> Option<f64> {
    let payload = http::get_json(client, sid, constants::BALANCE_PATH).await.ok()?;
    http::data_of(&payload)
        .get("total_balance")
        .and_then(Value::as_f64)
}

/// +08 时区的今天（`YYYY-MM-DD`）；上游任务里没给日期时才用。
fn today_cst() -> String {
    use chrono::{FixedOffset, Utc};
    let now = Utc::now();
    match FixedOffset::east_opt(8 * 3600) {
        Some(offset) => now.with_timezone(&offset).format("%Y-%m-%d").to_string(),
        None => now.format("%Y-%m-%d").to_string(),
    }
}

pub(super) fn trim_number(value: f64) -> String {
    let text = format!("{value:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_balance_into_usage() {
        let usage = shape_usage(&json!({
            "total_balance": 2306,
            "total_bonus": 2306,
            "total_purchased": 0,
            "bonus_credits": [
                { "balance": 706, "value": 800, "consumed": 94, "expire_time": "2026-11-16T15:27:10+08:00" }
            ],
            "purchased_credits": []
        }));
        assert_eq!(usage["available"], 2306.0);
        assert_eq!(usage["unit"], "智点");
        assert_eq!(usage["wallets"].as_array().map(Vec::len), Some(2));
        let credits = usage["credits"].as_array().expect("credits");
        assert_eq!(credits.len(), 1);
        assert_eq!(credits[0]["remain"], 706.0);
        assert!(credits[0]["expireAt"].as_i64().unwrap_or(0) > 0);
    }

    #[test]
    fn trims_trailing_zeros() {
        assert_eq!(trim_number(100.0), "100");
        assert_eq!(trim_number(100.5), "100.5");
    }
}
