use agent2api_provider_sdk::PluginCtx;
use reqwest::Method;
use serde_json::{json, Value};

use super::{client, endpoints, refresh};
use crate::errors::GatewayError;

pub async fn query(ctx: &PluginCtx, account: &str) -> Result<Value, String> {
    let data = refresh::protected(ctx, account, Method::GET, endpoints::SELF_PATH, None)
        .await
        .and_then(|reply| reply.data("积分查询"))
        .map_err(GatewayError::into_control_message)?;
    if !data.is_object() {
        return Err("QuWork 积分返回格式异常".into());
    }
    let entitlement = data.get("points_entitlement").unwrap_or(&Value::Null);
    // 账号 amount 的单位是百积分；points_entitlement 已经是积分，不能再次乘 100。
    let available = client::number(entitlement.get("total_points")).or_else(|| {
        client::number(data.get("amount"))
            .map(|amount| (amount * 100.0).round())
            .filter(|value| value.is_finite())
    });
    let mut credits = Vec::new();
    for (id, name, field) in [
        ("limited", "限时积分", "limited_points"),
        ("permanent", "永久积分", "permanent_points"),
    ] {
        if let Some(remaining) = client::number(entitlement.get(field)) {
            credits.push(json!({ "id": id, "name": name, "balance": remaining, "remain": remaining, "unit": "积分", "snapshot": true }));
        }
    }
    let subscription = entitlement
        .get("active_subscription")
        .filter(|value| value.is_object());
    let plan = subscription
        .and_then(|value| value.get("plan_name"))
        .and_then(Value::as_str);
    if let Some(subscription) = subscription {
        credits.push(json!({
            "id": "subscription", "name": plan.unwrap_or("订阅积分"),
            "remain": client::number(subscription.get("remaining_points")),
            "total": client::number(subscription.get("total_points")),
            "used": client::number(subscription.get("used_points")),
            "grantAt": client::timestamp(subscription.get("started_at")),
            "expireAt": client::timestamp(subscription.get("expires_at")),
            "unit": "积分",
        }));
    }
    if available.is_none() && credits.is_empty() {
        return Err("QuWork 未返回可识别的积分余额，未将未知余额当作 0".into());
    }
    let mut sources = Vec::new();
    if available.is_some() {
        sources.push(crate::quota::source(
            endpoints::PROVIDER,
            "account",
            "账号积分",
            "积分",
            available,
            None,
            None,
        ));
    }
    Ok(json!({
        "available": available, "unit": "积分", "plan": plan, "credits": credits,
        "until": subscription.and_then(|value| client::timestamp(value.get("expires_at"))),
        "quotaSources": sources,
    }))
}
