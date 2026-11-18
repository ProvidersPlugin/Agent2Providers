//! 当前客户端 GET /api/gateway/credit/balance，只读 availableCredits 与 userPlan。
//! 与对话共用 accessToken，但鉴权头是 X-Auth-Token；旧 balanceToken 仅缺主凭据时兜底。
//! 不发送网页积分中心 Cookie，不探测、不签到，不构造 total/frozen/expired 或 token usage。

use agent2api_wasm_sdk::{json, Result, Value};

use crate::{credentials, errors, network, util};

pub fn query() -> Result<Value> {
    let credentials = credentials::current()?;
    let payload = network::balance(&credentials)?;
    credentials::check_current(&credentials)?;
    let available = util::number(payload.get("availableCredits"));
    let subscription = subscription(&payload["userPlan"], &credentials.secrets());
    let wallets: Vec<Value> = available.into_iter().map(|balance| json!({
        "type": "available", "displayName": "可用积分", "balance": balance,
    })).collect();
    // 只返回已投影的业务字段；不把上游原始对象连同未知字段公开。
    let mut report = json!({ "available": available, "unit": "credits", "wallets": wallets,
        "subscription": subscription });
    if available.is_none() {
        report["message"] = json!("上游没有提供有效的可用积分读数，保留为空；未记为 0");
    }
    if credentials.token.is_empty() {
        report["credentialSource"] = json!("legacy_balance_fallback");
    }
    report["quotaSources"] = sources(&report);
    Ok(report)
}

fn subscription(plan: &Value, secrets: &[&str]) -> Value {
    if !plan.is_object() { return Value::Null; }
    let name = util::text(plan, &["planName", "planId"]);
    let mut result = json!({ "name": errors::public_text(&name, secrets, 100),
        "expireAt": null, "autoRenew": null, "pro": null });
    // 不解释或补造套餐到期时间：只接受上游给出的标量；缺失仍为空。
    if let Some(value) = plan.get("expireTime") {
        match value {
            Value::String(text) if text.len() <= 128 && !errors::contains_secret(text, secrets)
                && !text.chars().any(char::is_control) && !text.contains("://") => result["expireAt"] = json!(text),
            Value::Number(number) if number.as_i64().is_some_and(|value| value >= 0) => result["expireAt"] = json!(number),
            _ => {},
        }
    }
    for key in ["autoRenew", "pro"] {
        if let Some(value) = plan.get(key).and_then(util::flag) { result[key] = json!(value); }
    }
    result
}

pub fn sources(report: &Value) -> Value {
    match util::number(report.get("available")) {
        Some(remaining) => json!([{ "id": "account", "provider": "catpaw", "label": "可用积分",
            "unit": "credits", "remaining": remaining, "total": null, "used": null }]),
        None => json!([]),
    }
}
