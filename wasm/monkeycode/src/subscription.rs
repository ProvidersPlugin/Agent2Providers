//! 只读订阅字段。官方 ad169c9... 的开源后端固定返回 pro/auto_renew=false，
//! 不能搬成 SaaS 默认值；这里仅映射本次 GET 的已存在字段，缺失保持 null。

use agent2api_wasm_sdk::{json, Result, Value};
use chrono::DateTime;

use crate::{account::Snapshot, client, endpoints::ReadEndpoint, util};

pub fn read(account: &Snapshot) -> Result<Value> {
    account.unchanged()?;
    let result = client::get(&account.session, ReadEndpoint::Subscription, &[]);
    account.unchanged()?;
    let data = result?;
    let mut result = project(&data, &account.session);
    result["observedAt"] = json!(util::now()?);
    Ok(result)
}

fn field(data: &Value, key: &str, session: &str) -> Value {
    let text = util::text(data, &[key]);
    if text.is_empty() {
        Value::Null
    } else {
        json!(util::public_text(text, session, 256))
    }
}

fn timestamp(raw: &Value) -> Option<i64> {
    DateTime::parse_from_rfc3339(raw.as_str()?)
        .ok()
        .map(|at| at.timestamp_millis())
        .filter(|at| *at >= 0)
}

pub fn project(data: &Value, session: &str) -> Value {
    let plan = field(data, "plan", session);
    let expires = field(data, "expires_at", session);
    let period_end = field(data, "current_period_end", session);
    let grace = field(data, "grace_until", session);
    json!({
        "planName": plan,
        "plan": plan,
        "expiresAt": timestamp(&expires),
        "expires_at": expires,
        "currentPeriodEnd": timestamp(&period_end),
        "current_period_end": period_end,
        "graceUntil": timestamp(&grace),
        "grace_until": grace,
        "billingInterval": field(data, "billing_interval", session),
        "billingStatus": field(data, "billing_status", session),
        "paymentProvider": field(data, "payment_provider", session),
        "source": field(data, "source", session),
        "autoRenew": data["auto_renew"].as_bool(),
        "cancelAtPeriodEnd": data["cancel_at_period_end"].as_bool(),
        "enableCreditConsumption": data["enable_credit_consumption"].as_bool(),
        "readOnly": true
    })
}
