//! 签到只触发当天奖励结算，不代替套餐开通或协议确认。

use serde_json::{json, Value};

use agent2api_provider_sdk::PluginCtx;
use crate::errors::GatewayError;
use super::{control, subscription};

pub async fn claim_daily_checkin(
    ctx: &PluginCtx,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let previous = ctx.ledger_get(account_id, "dailyCheckin").await.ok();
    let result = claim_inner(ctx, account_id).await;
    let (completed, status, message) = match &result {
        Ok(value) => (
            true,
            if value.get("gained").and_then(Value::as_f64).unwrap_or(0.0) > 0.0 { "claimed" } else { "done" },
            value.get("msg").and_then(Value::as_str).unwrap_or("已签到").to_string(),
        ),
        Err(error) => (false, "failed", error.message.clone()),
    };
    let already_completed = previous.as_ref().is_some_and(|value| {
        value.get("completed").and_then(Value::as_bool) == Some(true)
            && value.get("at").and_then(Value::as_i64).and_then(crate::beijing::date_key_of_ms)
                .is_some_and(|day| day == crate::beijing::today_key())
    });
    if !completed && already_completed { return result; }
    let daily = json!({
        "kind": "daily", "at": crate::logging::now_ms(), "completed": completed,
        "status": status, "message": message, "completedCount": usize::from(completed),
        "totalCount": 1, "sourcesComplete": completed,
    });
    if let Err(error) = ctx.ledger_put(account_id, "dailyCheckin", &daily).await {
        crate::logging::verbose("[OfficeAce]", &format!("签到状态保存失败：{error}"));
    }
    result
}

async fn claim_inner(
    ctx: &PluginCtx,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let mut credential = control::credential_of(ctx, account_id).await?;
    let before = control::subscription_of(ctx, &mut credential).await?;
    match before.get("subscribe_status").and_then(Value::as_str) {
        Some("UNSUBSCRIBED") => return Err(GatewayError::with_status(
            409, "果办尚未开通体验套餐，请先到签到中心开通零元体验套餐，再领取奖励",
        ).with_code("officeace_subscription_required")),
        Some("EXPIRED" | "FROZEN") => return Err(GatewayError::with_status(
            409, "果办套餐已到期或被冻结，请到官方客户端处理；这里不会自动续费",
        )),
        Some("SUBSCRIBED") => {}
        _ => return Err(GatewayError::with_status(502, "无法确认果办套餐状态，未执行签到，请先查询套餐")),
    }
    control::resolve_project(ctx, &mut credential).await?;
    let claimed = control::claim_bonus(ctx, &mut credential).await?;
    let items = claimed.get("bonus_skus").and_then(Value::as_array)
        .ok_or_else(|| GatewayError::with_status(502, "官方未返回有效的奖励领取结果，请查询积分后再操作"))?;
    let bonuses = subscription::bonuses_of(&claimed);
    let gained: f64 = bonuses.iter().map(subscription::Bonus::remaining).sum();
    if items.len() != bonuses.len() || !gained.is_finite() || !items.is_empty() && gained <= 0.0 {
        return Err(GatewayError::with_status(502, "官方返回了奖励条目，但未能确认到账积分，请查询积分明细"));
    }
    super::onboarding::mark_claimed(ctx, account_id, &bonuses).await;
    let message = if gained > 0.0 {
        format!("已领取 {gained:.0} 积分")
    } else {
        "今天没有可领的奖励（或已领过）".to_string()
    };
    let rewards: Vec<Value> = bonuses.iter().map(|bonus| json!({
        "key": bonus.activity_id, "title": bonus.name, "ok": true,
        "already": false, "gained": bonus.remaining(),
    })).collect();
    Ok(json!({ "success": true, "msg": message, "gained": gained, "rewards": rewards }))
}
