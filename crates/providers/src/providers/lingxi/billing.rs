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
    if status == "claimed" {
        return Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Done,
            credit: None,
            streak,
            message: Some("今日已签到".to_string()),
        });
    }

    // 领取日期取任务里「今天可领」那一档的 date（+08 口径由上游给，别自己算）。
    let date = extra
        .get("check_in_days")
        .and_then(Value::as_array)
        .and_then(|days| {
            days.iter()
                .find(|day| day.get("status").and_then(Value::as_str) == Some("available"))
                .and_then(|day| day.get("date").and_then(Value::as_str))
        })
        .map(str::to_string)
        .unwrap_or_else(today_cst);

    let before = balance_total(&client, &sid).await.unwrap_or(0.0);
    let claim = http::post_json(
        &client,
        &sid,
        &format!("{}?date={}", constants::CLAIM_PATH, date),
        &json!({}),
        &[],
    )
    .await;
    if let Err(message) = claim {
        return Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Failed,
            credit: None,
            streak,
            message: Some(message),
        });
    }
    let after = balance_total(&client, &sid).await.unwrap_or(before);
    let delta = after - before;
    if delta > 0.0 {
        return Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Claimed,
            credit: Some(format!("+{} 智点", trim_number(delta))),
            streak: streak.map(|value| value + 1),
            message: None,
        });
    }
    // 返回 ok 但余额没动：上游幂等，今天已经领过
    Ok(proto::CheckinReport {
        outcome: proto::CheckinOutcome::Done,
        credit: None,
        streak,
        message: Some("今日已签到（上游幂等，余额未变化）".to_string()),
    })
}

async fn balance_total(client: &reqwest::Client, sid: &str) -> Option<f64> {
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

fn trim_number(value: f64) -> String {
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
