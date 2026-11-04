//! AStudio 余额：网页会话读 `points/balance` 与 `membership/me`。
//!
//! ── 两种账号形态 ────────────────────────────────────────────
//! ```text
//!   有网页会话  →  通用积分 / 星火积分 / 会员档 / 到期时间（真实读数）
//!   只有模型凭据 →  用目录接口探活，显示「AStudio」与一个连通性结果
//! ```
//! 所有读取都不计费；只有模型调用才消耗积分。
//!
//! ── 单项失败不塌方 ──────────────────────────────────────────
//! 余额与会员信息是两条接口：一条挂了另一条还能显示。但**会话失效必须
//! 透传**（否则用户看到的是「账号信息读取失败」，而不是「请重新登录」）。

use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::accounts::Credentials;
use super::client::{self, CatalogError, StudioError};
use super::{endpoints, PROVIDER_ID};

fn number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(|value| value.as_f64())
        .filter(|number| number.is_finite())
}

/// 查一次余额。
pub async fn query(ctx: &PluginCtx, account: &str) -> Result<Value, String> {
    let stored = ctx
        .store_get(PROVIDER_ID, account)
        .await
        .map_err(|error| format!("读取 AStudio 账号失败：{error}"))?;
    let credentials = Credentials::from_secret(&stored.secret);
    if credentials.key.is_empty() {
        return Err("AStudio 账号缺少 modelBearerToken，请重新登录或粘贴".to_string());
    }
    if !credentials.has_session() {
        // 只有模型凭据：用目录接口探活（与参考实现的 usage 同处置）。
        return match client::fetch_catalog(&credentials.key).await {
            Ok(_) => Ok(json!({ "provider": PROVIDER_ID, "plan": "AStudio", "windows": [] })),
            Err(CatalogError::Expired) => {
                Err("AStudio 拒绝了这份凭据（HTTP 401/403），请重新登录".to_string())
            }
            Err(CatalogError::Failed(message)) => Ok(json!({
                "provider": PROVIDER_ID,
                "plan": "AStudio",
                "windows": [],
                "error": format!("AStudio 不可达：{message}"),
            })),
        };
    }
    let (balance, membership) = tokio::join!(
        client::studio_json(&credentials, endpoints::POINTS_BALANCE, "积分余额"),
        client::studio_json(&credentials, endpoints::MEMBERSHIP_ME, "会员信息"),
    );
    let mut expired: Option<String> = None;
    let mut failures: Vec<String> = Vec::new();
    let balance = match balance {
        Ok(value) => Some(value),
        Err(StudioError::Expired(message)) => {
            expired = Some(message);
            None
        }
        Err(error) => {
            failures.push(error.message().to_string());
            None
        }
    };
    let membership = match membership {
        Ok(value) => Some(value),
        Err(StudioError::Expired(message)) => {
            expired = Some(message);
            None
        }
        Err(error) => {
            failures.push(error.message().to_string());
            None
        }
    };
    if let Some(message) = expired {
        return Err(message);
    }
    if balance.is_none() && membership.is_none() {
        return Ok(json!({
            "provider": PROVIDER_ID,
            "plan": "AStudio",
            "windows": [],
            "error": failures.join("；"),
        }));
    }
    let mut windows: Vec<Value> = Vec::new();
    if let Some(balance) = &balance {
        if let Some(total) = number(balance.get("totalBalance")) {
            let mut window = json!({
                "name": "通用积分",
                "label": "通用积分",
                "balance": total,
                "remaining": total,
                "display": format_number(total),
                "unit": "积分",
                "aside": false,
            });
            if let Some(activity) = number(balance.get("activityBalance")) {
                if activity > 0.0 {
                    if let Some(next) = balance
                        .get("activityNextExpireTime")
                        .filter(|value| !value.is_null())
                    {
                        window["runsOutAt"] = next.clone();
                        window["runsOut"] = json!(activity);
                    }
                }
            }
            windows.push(window);
        }
        if let Some(spark) = number(balance.get("sparkTotalBalance")) {
            windows.push(json!({
                "name": "星火积分",
                "label": "星火积分",
                "balance": spark,
                "remaining": spark,
                "display": format_number(spark),
                "unit": "积分",
                "aside": false,
            }));
        }
    }
    let mut report = json!({
        "provider": PROVIDER_ID,
        "unit": "积分",
        "windows": windows,
    });
    if let Some(membership) = &membership {
        if let Some(plan) = membership.get("planName").and_then(Value::as_str) {
            if !plan.is_empty() {
                report["plan"] = json!(plan);
            }
        }
        if let Some(uid) = membership.get("uid") {
            if !uid.is_null() {
                report["user"] = json!(uid.to_string());
            }
        }
        if let Some(until) = membership.get("endTime").filter(|value| !value.is_null()) {
            report["until"] = until.clone();
        }
    }
    if report.get("plan").is_none() {
        report["plan"] = json!("AStudio");
    }
    Ok(report)
}

fn format_number(value: f64) -> String {
    if value.fract().abs() < 0.005 {
        format!("{}", value.round() as i64)
    } else {
        format!("{value:.2}")
    }
}
