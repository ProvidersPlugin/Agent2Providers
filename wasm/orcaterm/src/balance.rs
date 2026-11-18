//! 当前 AI 配额独立读数；旧 billing 只作明确缺路由的兼容，不硬编码周期或套餐额度。

use super::{account, errors, upstream, ID};
use agent2api_wasm_sdk::{json, Error, Result, Value};

fn counter(value: &Value) -> Result<Option<f64>> {
    if value.is_null() {
        return Ok(None);
    }
    let number = value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|number| number.is_finite())
        .ok_or_else(|| Error::http(502, "OrcaTerm 额度读数格式无效"))?;
    Ok(Some(number))
}

pub fn query() -> Result<Value> {
    let credentials = account::ensure()?;
    let billing = match upstream::cgi(Some(&credentials), "DescribeUserBillingStatus", json!({})) {
        Ok(response) => response,
        Err(error) if matches!(error.status, Some(404 | 410)) => Value::Null,
        Err(error) => return Err(error),
    };
    if errors::business_failed(&billing) {
        return Err(errors::upstream(
            200,
            &billing,
            &[&credentials.access_token],
        ));
    }
    let response = match billing["usageStats"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| item["feature"] == "hunyuan_ai")
    {
        Some(item) => item.clone(),
        None => upstream::cgi(
            Some(&credentials),
            "DescribeUserUsageStats",
            json!({ "feature": "hunyuan_ai" }),
        )?,
    };
    if errors::business_failed(&response) {
        return Err(errors::upstream(
            200,
            &response,
            &[&credentials.access_token],
        ));
    }
    let ai = response
        .get("usageStats")
        .and_then(Value::as_array)
        .and_then(|items| items.iter().find(|item| item["feature"] == "hunyuan_ai"))
        .unwrap_or(&response);
    if ai
        .get("feature")
        .is_some_and(|feature| feature != "hunyuan_ai")
    {
        return Err(Error::http(502, "OrcaTerm 返回的额度不是 AI 助手配额"));
    }
    let limit = counter(&ai["quotaLimit"])?;
    let used = counter(&ai["quotaUsed"])?;
    if limit.is_none() && used.is_none()
        || used.is_some_and(|used| used < 0.0)
        || limit.is_some_and(|limit| limit < 0.0 && limit != -1.0)
    {
        return Err(Error::http(
            502,
            "OrcaTerm AI 额度读数缺失或无效，未当作零额度",
        ));
    }
    let unlimited = limit == Some(-1.0);
    let total = limit.filter(|limit| *limit >= 0.0);
    let available = total.zip(used).map(|(limit, used)| (limit - used).max(0.0));
    let reset = ai["quotaEndDate"]
        .as_str()
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map(|date| date.timestamp_millis())
        .filter(|at| *at > 0);
    let view = if unlimited {
        "不限次数".to_string()
    } else {
        total
            .zip(available)
            .map(|(limit, available)| format!("{available}/{limit} 次"))
            .unwrap_or_default()
    };
    let plan_name = match billing["roleType"].as_str().unwrap_or("") {
        "basic" => "基础版",
        "professional" => "专业版",
        name => name,
    };
    account::validate_current(&credentials)?;
    Ok(json!({
        "available": available, "unit": "次", "availableView": view, "unlimited": unlimited,
        "subscription": { "planName": plan_name, "status": "active", "expireAt": reset,
            "remainQuota": available, "totalQuota": total, "unlimited": unlimited },
        "quotaSources": [{ "id": "hunyuan_ai", "label": "AI 助手额度", "provider": ID,
            "unit": "次", "remaining": available, "total": total, "used": used, "unlimited": unlimited }]
    }))
}
