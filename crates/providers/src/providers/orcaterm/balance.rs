//! OrcaTerm 余额：`DescribeUserBillingStatus` 的月度 AI 配额 → 统一 usage 形状。
//!
//! ── 上游形态（2026-10-16 实测）──────────────────────────────
//! ```jsonc
//! { "roleType": "basic", "usageStats": [
//!     { "feature": "hunyuan_ai", "quotaLimit": 1000, "quotaUsed": 0,
//!       "quotaStartDate": "…", "quotaEndDate": "2026-11-15T16:00:00.000Z" }, … ] }
//! ```
//! 配额按自然月重置；`hunyuan_ai` 就是 AI 助手的对话额度（基础版 1000/月、
//! 专业版 6000/月）。其余（session_share / secret / space）不是推理额度，
//! 不进 usage。
//!
//! ── 归一形状 ────────────────────────────────────────────────
//! `available` = limit - used、`unit` = "次"、`subscription` 带套餐档与
//! 重置时刻（`roleType` 映射展示名，其余原样透出 —— 套餐名以上游原文为准，
//! 不引入本地码表）。

use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::credentials;

/// AI 助手配额在 `usageStats[].feature` 里的取值。
const HUNYUAN_FEATURE: &str = "hunyuan_ai";

/// 查询某账号的 AI 配额。
pub async fn query_usage(ctx: &PluginCtx, account: &str) -> Result<Value, String> {
    let client = crate::http::control_client(None, &ctx.env.proxy);
    let mut credentials = credentials::snapshot(ctx, account).await?;
    credentials = credentials::refresh(ctx, &credentials, account, false).await?;
    let response = credentials::cgi_action(
        &client,
        &credentials,
        "DescribeUserBillingStatus",
        json!({}),
    )
    .await
    .map_err(|error| error.message)?;
    if let Some(error) = response.get("Error") {
        let code = error.get("Code").and_then(Value::as_str).unwrap_or("UNKNOWN");
        let message = error.get("Message").and_then(Value::as_str).unwrap_or("上游错误");
        return Err(format!("OrcaTerm 余额查询被上游拒绝（{code}：{message}）"));
    }
    report_of(&response)
}

/// 把上游响应归一成 usage 形状。
fn report_of(response: &Value) -> Result<Value, String> {
    let role_type = response.get("roleType").and_then(Value::as_str).unwrap_or("");
    let stats = response
        .get("usageStats")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let ai = stats
        .iter()
        .find(|item| item.get("feature").and_then(Value::as_str) == Some(HUNYUAN_FEATURE))
        .ok_or_else(|| {
            "OrcaTerm 余额响应里没有 AI 助手配额项（hunyuan_ai）".to_string()
        })?;
    let limit = ai.get("quotaLimit").and_then(Value::as_i64).unwrap_or(0);
    let used = ai.get("quotaUsed").and_then(Value::as_i64).unwrap_or(0);
    let available = if limit > used { limit - used } else { 0 };
    let reset_at = iso_to_ms(ai.get("quotaEndDate").and_then(Value::as_str));
    let plan_name = match role_type {
        "basic" => "基础版",
        "professional" => "专业版",
        other => other,
    };
    Ok(json!({
        "available": available,
        "unit": "次",
        "availableView": if limit > 0 { format!("{available}/{limit} 次") } else { String::new() },
        "subscription": {
            "planName": plan_name,
            "status": "active",
            "expireAt": reset_at,
            "remainQuota": available,
            "totalQuota": limit,
        },
        "raw": response,
    }))
}

/// ISO-8601 时间串 → 毫秒时间戳（解析不出给 0）。
fn iso_to_ms(text: Option<&str>) -> i64 {
    let Some(text) = text else { return 0 };
    chrono::DateTime::parse_from_rfc3339(text)
        .map(|value| value.timestamp_millis())
        .unwrap_or(0)
}
