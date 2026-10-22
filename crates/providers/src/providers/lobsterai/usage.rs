//! LobsterAI 的余额读数：`GET /api/user/profile-summary` 的**积分**。
//!
//! ── 为什么不用 `/api/user/quota`（一条实测教训）──────────────────
//! 那个端点只报 `freeCreditsTotal`（实测恒为 **300**），完全不含活动 / 签到
//! 得到的积分；参考实现在同一账号上量到的真实余额是 **5297.72**，与 300 差一个
//! 数量级（`credits.go:370-372` 的原话：「it deliberately uses profile-summary
//! rather than /api/user/quota, which only reports the free credit total and omits
//! activity credits」）。用错端点会让界面稳定显示一个偏小的数，且**不报错**。
//!
//! ── 读什么 ──────────────────────────────────────────────────
//! ```text
//!   data.totalCreditsRemaining   总剩余积分
//!   data.creditItems[]           逐份积分：{type, creditsRemaining, expiresAt}
//! ```
//! 单位是**积分**。负数要夹到 0（计量回滚会让服务端报负数，显示「-12.5 积分」
//! 没有意义，`credits.go:413-415`）。
//!
//! ── 归一形状（与 loomy/balance.rs 同款，界面真正读的那一套）──────
//! ```text
//!   { available, unit, wallets: [{type, displayName, balance, remaining, expiresAt}], raw }
//! ```
//! ⚠️ 曾经有一家（灵犀）在这处自造了 `total_balance` / `currency` 两个键，
//! 结果账号页余额列**空白**而探针还全绿。所以这里**严格照抄**别家的键名，不自造。
//!
//! ── 一条**必须**遵守的契约：读不到额度就不要写 `available` ──────
//! 限额判定走 `core::usage_records::extract_remaining`，规则是「`unlimited`
//! 优先，否则看 `totalLeft`，否则看 `available`」—— **`available` 一旦出现，
//! 就会被当成真实读数**。2026-10-17 在 atomcode 上栽过一次：没有读数时也写
//! `available: 0`，于是「读不到额度」被限制器理解成「余额为 0」，账号当场被
//! 标成「余额不足 / 已跳过」。所以这里只在**确实解析出读数**时才写
//! `available`（`totalCreditsRemaining` 有值，或至少有一份明细可求和）；
//! 什么都没有时只回 `wallets: []`，余额列留空。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{Value, json};

use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;
use crate::http::send_raw;
use crate::logging;

use super::credentials::from_record;
use super::endpoints::{REQUEST_TIMEOUT_MS, USER_AGENT, profile_summary_url};
use super::oauth::unwrap_envelope;

/// 保留两位小数（服务端的浮点噪声如 `55.67000031` 在几份求和后会显形）
fn round_credits(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// 夹到非负（计量回滚会让服务端报负数）
fn clamp_credits(value: f64) -> f64 {
    round_credits(value.max(0.0))
}

/// 读一个数字字段（数字或数字字符串都认）
fn read_number(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    if let Some(number) = value.as_f64() {
        return Some(number);
    }
    value.as_str().and_then(|text| text.trim().parse::<f64>().ok())
}

/// 一份积分明细（`creditItems[]` 的一条）
struct CreditItem {
    kind: String,
    remaining: f64,
    expires_at: String,
}

/// 取明细数组（非数组 / 条目不是对象一律跳过，不 panic）
fn credit_items(payload: &Value) -> Vec<CreditItem> {
    let Some(items) = payload.get("creditItems").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let entry = item.as_object()?;
            let kind = entry
                .get("type")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .unwrap_or("积分包")
                .to_string();
            Some(CreditItem {
                remaining: clamp_credits(read_number(entry.get("creditsRemaining")).unwrap_or(0.0)),
                expires_at: entry
                    .get("expiresAt")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or("")
                    .to_string(),
                kind,
            })
        })
        .collect()
}

/// 把 `profile-summary` 的 `data` 归成界面认的额度形状（**纯函数**，便于测试）。
///
/// 契约见模块头：读不到读数就**不写 `available`**。
pub fn shape_usage(payload: &Value) -> Value {
    let items = credit_items(payload);
    let total = read_number(payload.get("totalCreditsRemaining"));

    // 诊断：把上游的真实字段形态记进**详细日志**（额度按字段名解析，名字一变就
    // 静默归零）。只记键名与明细，都不含凭据，且不进普通日志。
    logging::verbose(
        "[LobsterAI]",
        &format!(
            "profile-summary 形态：顶层键={:?} creditItems={}",
            payload
                .as_object()
                .map(|object| object.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default(),
            payload
                .get("creditItems")
                .map(|value| serde_json::to_string(value).unwrap_or_default())
                .map(|text| text.chars().take(400).collect::<String>())
                .unwrap_or_else(|| "<缺>".to_string()),
        ),
    );

    let wallets: Vec<Value> = items
        .iter()
        .map(|item| {
            let mut wallet = json!({
                "type": "credit_package",
                "displayName": item.kind,
                "balance": item.remaining,
                "remaining": item.remaining,
            });
            if let Some(object) = wallet.as_object_mut() {
                if !item.expires_at.is_empty() {
                    object.insert(
                        "expiresAt".to_string(),
                        Value::String(item.expires_at.clone()),
                    );
                }
            }
            wallet
        })
        .collect();

    // 读数优先级：`totalCreditsRemaining`（权威总量）→ 明细求和（仍是真实读数）
    let available = total.map(clamp_credits).or_else(|| {
        if items.is_empty() {
            None
        } else {
            Some(clamp_credits(items.iter().map(|item| item.remaining).sum()))
        }
    });

    // ── `credits[]`：界面「积分明细」那一列读的就是它 ──────────────
    // 契约见 `ui-islands/src/islands/accounts-credit-detail.tsx` 的模块头：
    // `{name, total/used/remain, grantAt, startAt, expireAt, unit?, daily?, grantReason?}`，
    // **缺键 = 上游没给，不补 0**（把「没读到」写成 0 会画出「剩 0 / 共 0」这种
    // 看起来像读到了的假读数）。
    //
    // 本家只报剩余量：上游的明细条目只有 `type` 与 `creditsRemaining`
    // （Go 参考 `credits.go:387-410` 明确把 `Total`/`Used` 留成 0，理由是
    // 「上游没有周期总量/已用的概念」）。所以这里也只写 `name` 与 `remain`。
    // ⚠️ 到期那一栏（`expireAt`）**故意不写**：上游给的是**字符串**时刻，
    // 而界面这一栏要的是**毫秒数**，字符串的格式我们没核实过 —— 宁可这一格留空，
    // 也不要把一个日期串塞进毫秒字段（那会渲染成一个 1970 年的时刻）。
    // `wallets[].expiresAt` 是另一条路径（那是我们自己的形状，保留原文）。
    let credits: Vec<Value> = items
        .iter()
        .map(|item| {
            json!({
                "name": if item.kind.trim().is_empty() { "积分包" } else { item.kind.as_str() },
                "remain": item.remaining,
            })
        })
        .collect();

    let mut shaped = json!({
        "unit": "积分",
        "wallets": wallets,
        "credits": credits,
        "raw": payload,
    });
    // 只有真有读数才回 `available`（见模块头：写成 0 会被限制器当成「余额为 0」）
    if let Some(available) = available {
        if let Some(object) = shaped.as_object_mut() {
            object.insert("available".to_string(), Value::from(available));
        }
    }
    shaped
}

/// 查余额：读记录 → 取 `profile-summary` → 解信封 → 归一。
pub async fn query_usage(
    ctx: &PluginCtx,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let record = crate::store::record(ctx, super::ID, account_id).await?;
    let credentials = from_record(Some(&record))?;

    let headers: Vec<(String, String)> = vec![
        (
            "authorization".to_string(),
            format!("Bearer {}", credentials.access_token),
        ),
        ("accept".to_string(), "application/json".to_string()),
        ("user-agent".to_string(), USER_AGENT.to_string()),
    ];
    let response = send_raw(
        "GET",
        &profile_summary_url(),
        None,
        &headers,
        None,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await
    .map_err(|error| {
        if error.is_timeout() {
            GatewayError::with_status(504, "LobsterAI 积分查询超时")
        } else {
            GatewayError::with_status(502, format!("LobsterAI 积分查询失败: {error}"))
        }
    })?;
    let payload = response.payload.ok_or_else(|| {
        GatewayError::with_status(502, "LobsterAI 积分查询响应不是 JSON")
    })?;
    // 401/403 原样透出（调用方据此走「刷新后重试一次」的既有处置）
    if response.status == 401 || response.status == 403 {
        return Err(GatewayError::with_status(
            401,
            format!("LobsterAI 积分查询被拒（HTTP {}），请重新登录", response.status),
        ));
    }
    if !response.ok {
        return Err(GatewayError::with_status(
            response.status as i32,
            format!("LobsterAI 积分查询返回 HTTP {}", response.status),
        ));
    }
    // 信封：`code:0` 且 `data` 非 null 才算成功（`data:null` = 登录态失效）
    let data = unwrap_envelope(&payload, "积分查询")?;
    Ok(shape_usage(&data))
}
