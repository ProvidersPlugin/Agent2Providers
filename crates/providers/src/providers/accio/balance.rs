//! Accio 额度查询：归一成账号页的统一形状（`ProviderAdapter::query_usage` 契约）。
//!
//! ── 上游给的是什么 ──────────────────────────────────────────
//! `GET /api/entitlement/quota?accessToken=…` → `{usagePercent, refreshCountdownSeconds}`
//! （桌面端本地路由 `/auth/quota` 就是把它算成这两个数给界面用的）。
//! 另有 `GET /api/entitlement/currentSubscription?accessToken=…` 给订阅详情
//! （套餐名 / 到期时间 / 剩余额度），它可能不存在（免费账号），失败就当没有。
//!
//! ── 双限额：百分比 + 积分 ────────────────────────────────────
//! Accio 的额度是**两重闸门**，两个都要看：
//!   · `/api/entitlement/quota` → `usagePercent`：百分比窗口，`refreshCountdownSeconds`
//!     后重置（实测 24 小时）。这是「这一轮还能不能用」。
//!   · `currentSubscription` → `remainQuota` / `totalQuota`：积分余额（实测国内版
//!     600 / 600）。这是「账户里还剩多少额度」，花完不会随窗口重置。
//! 只显示百分比会让用户看不到积分余量，而积分才是会被真正扣完的那一侧。
//! 因此 `wallets` 里两条都给：`额度余量`（%）与 `积分`（绝对数）。
//!
//! ── 为什么 available 给 null ────────────────────────────────
//! `/quota` 不暴露「还剩多少积分」的绝对数，只有一个**已用百分比**；而积分
//! 只在订阅接口里、可能整条不存在（免费账号）。`available` 是「可用总量」这
//! 一个数的位置，两种单位混不进同一个字段，所以按契约给 null（判不出就给 null），
//! 前端会退到「读第一条可读钱包」的口径 —— 也就是额度余量百分比。
//!   - `unit` = `"%"`；
//!   - `wallets[0]` = 「额度余量」= `100 - usagePercent`，重置倒计时写进 `detail`；
//!   - `wallets[1]` = 「积分」，仅在订阅接口真给了 `remainQuota` 时出现；
//!   - `subscription` 用 `remainQuota / totalQuota` 承载同一件事，
//!     免得前端只认 subscription 时什么都显示不出来。
//!
//! ── 401 为什么原样透出 ──────────────────────────────────────
//! 契约要求（见 `ProviderAdapter::query_usage`）：调用方据此走「刷新后重试一次」。

use serde_json::{json, Value};


use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;

use super::auth;
use super::endpoints;

/// 查一个账号的额度 / 订阅，归一成统一形状。
///
/// `region` 必给：两个地区是两家 provider，取记录时按它过滤。
pub async fn query(
    ctx: &PluginCtx,
    account_id: &str,
    region: super::endpoints::Region,
) -> Result<Value, GatewayError> {
    let record = crate::store::record(ctx, region.provider_id(), account_id).await?;
    if !record.is_object() {
        return Err(GatewayError::with_status(404, "Accio 账号不存在，请先添加账号"));
    }
    let credentials = auth::snapshot(&record)?;
    let proxy = auth::account_proxy(&record)?;

    // ── 额度用量 ────────────────────────────────────────────────
    let quota_response = auth::get_with_token(
        credentials.region,
        endpoints::QUOTA_PATH,
        &credentials.access_token,
        &[],
        proxy.as_ref(),
    )
    .await?;
    if quota_response.status == 401 || quota_response.status == 403 {
        return Err(GatewayError::with_status(401, "Accio 凭证已失效，请重新登录"));
    }
    let quota = auth::payload(quota_response, "额度查询").map(auth::unwrap_data)?;
    let usage_percent = quota
        .get("usagePercent")
        .or_else(|| quota.get("usage_percent"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        .clamp(0.0, 100.0);
    let countdown = quota
        .get("refreshCountdownSeconds")
        .or_else(|| quota.get("refresh_countdown_seconds"))
        .and_then(Value::as_f64);
    let remain = (100.0 - usage_percent).max(0.0);

    // ── 订阅（可选，失败不影响额度的展示）──────────────────────
    let subscription = match auth::get_with_token(
        credentials.region,
        endpoints::SUBSCRIPTION_PATH,
        &credentials.access_token,
        &[],
        proxy.as_ref(),
    )
    .await
    {
        Ok(response) if response.ok => response
            .payload
            .map(auth::unwrap_data)
            .map(|value| subscription_shape(&value))
            .unwrap_or(Value::Null),
        // 免费账号没有订阅：失败当「没有」，不影响额度那一半的展示
        _ => Value::Null,
    };

    let mut wallet = json!({
        "type": "entitlement",
        "displayName": "额度余量",
        "balance": remain,
        "unit": "%",
        "usedPercent": usage_percent,
    });
    if let Some(seconds) = countdown.filter(|value| *value > 0.0) {
        wallet["detail"] = Value::String(format!("{} 后重置", humanize_seconds(seconds)));
    }
    let mut wallets = vec![wallet];

    // ── 积分（第二重限额）────────────────────────────────────
    // Accio 是**双限额**：`/quota` 给的是百分比闸门，`currentSubscription`
    // 里的 `remainQuota` / `totalQuota` 是积分闸门（实测国内版 600 / 600）。
    // 只显示百分比会让「还剩多少积分」这件事完全看不到 —— 而积分是真正会
    // 被扣完的那一侧，百分比窗口每天还会重置。
    //
    // 只在上游确实给了积分读数时才加这一行：免费账号没有订阅、或上游换了
    // 字段名时，凭空补一条「积分 0」比不显示更糟（会被读成「已经用完了」）。
    let credits = credit_reading(&subscription);
    if let Some((remain_credits, total_credits)) = credits {
        let mut wallet = json!({
            "type": "credits",
            "displayName": "积分",
            "balance": remain_credits,
            "unit": "积分",
        });
        if let Some(total) = total_credits {
            wallet["total"] = json!(total);
            // 上游给了总量就一并报已用比例：明细面板能并排显示两个数。
            if total > 0.0 {
                wallet["usedPercent"] = json!(((total - remain_credits) / total * 100.0).clamp(0.0, 100.0));
            }
        }
        wallets.push(wallet);
    }

    Ok(json!({
        // 上游的 `/quota` 只有百分比、没有绝对剩余量，所以 `available` 给 null：
        // 两种单位（% 与积分）混不进同一个数值字段。余额列读的是下面那个
        // `availableView`，明细面板读 `wallets`。
        "available": Value::Null,
        // 余额列只有几十像素宽、只画一行，所以这一家的两重限额在这里合成一句
        // （「可用 100% · 600 积分」）。不带积分时退回纯百分比，与改造前逐字相同。
        "availableView": match credits {
            Some((remain_credits, _)) => format!("{remain:.0}% · {} 积分", compact_number(remain_credits)),
            None => format!("{remain:.0}%"),
        },
        "unit": "%",
        "wallets": wallets,
        "subscription": subscription,
        "raw": {
            "quota": quota,
            "region": credentials.region.edition(),
            "account": credentials.user_id,
        },
    }))
}

/// 从 `subscription` 形状里取积分读数：`(剩余, 总量)`。
///
/// 判据是**结构**不是 provider：`remainQuota` 存在且是有限数才算读到。
/// 总量缺失或为 0 时只回剩余（不编造分母）。
fn credit_reading(subscription: &Value) -> Option<(f64, Option<f64>)> {
    let remain = subscription.get("remainQuota").and_then(Value::as_f64)?;
    if !remain.is_finite() || remain < 0.0 {
        return None;
    }
    let total = subscription
        .get("totalQuota")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0);
    Some((remain, total))
}

/// 订阅响应 → 契约里的 `subscription` 形状（字段名宽口径）。
fn subscription_shape(payload: &Value) -> Value {
    let text = |keys: &[&str]| -> Value {
        keys.iter()
            .find_map(|key| payload.get(*key).and_then(Value::as_str))
            .filter(|value| !value.trim().is_empty())
            .map(|value| Value::String(value.to_string()))
            .unwrap_or(Value::Null)
    };
    let number = |keys: &[&str]| -> Value {
        keys.iter()
            .find_map(|key| payload.get(*key).and_then(Value::as_f64))
            .map(|value| json!(value))
            .unwrap_or(Value::Null)
    };
    let expire = ["expireAt", "expiresAt", "endTime", "end_time"]
        .iter()
        .find_map(|key| payload.get(*key))
        .cloned()
        .unwrap_or(Value::Null);
    json!({
        "planName": text(&["planName", "plan_name", "productName", "name"]),
        "status": text(&["status", "state"]),
        "expireAt": expire,
        "remainQuota": number(&["remainQuota", "remain", "remaining", "leftQuota"]),
        "totalQuota": number(&["totalQuota", "total", "quota"]),
    })
}

/// 积分读数 → 紧凑展示串（`600` / `1.2 万`）。
///
/// 余额列只有几十像素宽，五位以上的整数会把它撑爆；万位以上折成「万」，
/// 与界面别处（`numberText`）的紧凑口径一致。整数直接不带小数点。
fn compact_number(value: f64) -> String {
    if !value.is_finite() {
        return "0".to_string();
    }
    let value = value.max(0.0);
    if value >= 10_000.0 {
        format!("{:.1} 万", value / 10_000.0)
    } else if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

/// 秒 → 「3 小时 20 分」这类人话（界面直接显示）
fn humanize_seconds(seconds: f64) -> String {
    let total = seconds.max(0.0) as i64;
    let days = total / 86_400;
    let hours = (total % 86_400) / 3_600;
    let minutes = (total % 3_600) / 60;
    if days > 0 {
        format!("{days} 天 {hours} 小时")
    } else if hours > 0 {
        format!("{hours} 小时 {minutes} 分")
    } else if minutes > 0 {
        format!("{minutes} 分")
    } else {
        format!("{total} 秒")
    }
}
