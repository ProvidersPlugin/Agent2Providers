//! 国际订阅只读映射。本地官方固定开源版返回 pro，不能作为线上默认套餐。
//! 日期仅接受 RFC3339；未知字段、期限、真假值为 null，不推算。

use crate::{budget::Budget, client, endpoints::ReadEndpoint, snapshot::Snapshot, util};
use agent2api_wasm_sdk::{json, Result, Value};
use chrono::DateTime;

fn timestamp(value: Option<&str>) -> Option<i64> {
    DateTime::parse_from_rfc3339(value?)
        .ok()
        .map(|time| time.timestamp_millis())
        .filter(|time| *time >= 0 && *time <= util::MAX_SAFE_INTEGER)
}

pub fn read(account: &Snapshot, budget: &Budget) -> Result<Value> {
    account.unchanged()?;
    let result = client::get(&account.session, ReadEndpoint::Subscription, &[], budget);
    account.unchanged()?;
    let data = result?.data;
    let mut guard = account.guard.clone();
    guard.observe(&data)?;
    let plan = guard.field(&data, "plan", 128);
    let expires = guard.field(&data, "expires_at", 256);
    let period = guard.field(&data, "current_period_end", 256);
    let grace = guard.field(&data, "grace_until", 256);
    let mut warnings =
        vec!["订阅协议有官方源码依据，当前国际部署未验收；没有使用开源默认 pro 套餐".to_string()];
    if plan.is_none() {
        warnings.push("订阅未返回有效套餐名，保持未知".into());
    }
    for key in ["expires_at", "current_period_end", "grace_until"] {
        if !data[key].is_null() && timestamp(data[key].as_str()).is_none() {
            warnings.push(format!("订阅 {key} 期限格式未知，未推算或转换"));
        }
    }
    for key in [
        "auto_renew",
        "cancel_at_period_end",
        "enable_credit_consumption",
    ] {
        if !data[key].is_null() && !data[key].is_boolean() {
            warnings.push(format!("订阅 {key} 状态格式未知"));
        }
    }
    Ok(json!({
        "planName": plan, "plan": plan, "expires_at": expires, "expiresAt": timestamp(expires.as_deref()),
        "current_period_end": period, "currentPeriodEnd": timestamp(period.as_deref()),
        "grace_until": grace, "graceUntil": timestamp(grace.as_deref()),
        "billingInterval": guard.field(&data, "billing_interval", 128),
        "billingStatus": guard.field(&data, "billing_status", 128),
        "paymentProvider": guard.field(&data, "payment_provider", 128),
        "source": guard.field(&data, "source", 128),
        "autoRenew": data["auto_renew"].as_bool(), "cancelAtPeriodEnd": data["cancel_at_period_end"].as_bool(),
        "enableCreditConsumption": data["enable_credit_consumption"].as_bool(),
        "readOnly": true, "internationalVerified": false, "observedAt": util::now()?, "warnings": warnings,
        "state": if plan.is_some() { "observed_unverified" } else { "unknown" }
    }))
}
