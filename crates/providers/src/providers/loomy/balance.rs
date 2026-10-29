//! Loomy 积分 / 余额查询：`GET {集成网关}/api/v2/points/records` 的摘要字段。
//!
//! ── 为什么余额藏在流水接口里（客户端就是这么读的）──────────────
//! Loomy **没有独立的余额接口**。客户端的积分详情页读的是 V2 流水接口 ——
//! 它的响应 `data` 里同时带账户摘要：
//!
//! ```text
//! GET /api/v2/points/records?pageNo=1&pageSize=1&direction=debit&granularity=chat
//!   → {code:"000000", data:{ balance,           ← 永久积分（长期积分）
//!                          dailyBalance,       ← 每日赠送积分（每日登录刷回 5000）
//!                          dailyLimitPoints, dailyConsumedPoints, dailyRemainingPoints,
//!                          list:[…], total }}
//! ```
//!
//! 这里按账号页的统一形状（`{available, unit, wallets, credits, raw}`）归一化：
//! 两个钱包各一行，`available` 取两者之和（实际可用的总量）。

use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::client;
use super::credentials;

/// 流水查询路径（相对集成网关；pageSize 取 1 只为拿摘要，不看流水本身）
const RECORDS_PATH: &str =
    "/api/v2/points/records?pageNo=1&pageSize=1&direction=debit&granularity=chat";

/// 从 `data` 里取一个非负数字（字段缺失 / 非法时给 None）
fn pick_number(data: &Value, key: &str) -> Option<f64> {
    let value = data.get(key)?;
    if let Some(number) = value.as_f64() {
        return Some(number);
    }
    // 上游有把数字序列化成字符串的形态（如 "5000"）
    value
        .as_str()
        .and_then(|text| text.trim().parse::<f64>().ok())
}

/// 查询某 Loomy 账号的双账户余额。
pub async fn query_usage(ctx: &PluginCtx, account: &str) -> Result<Value, String> {
    let client = crate::http::control_client(None, &ctx.env.proxy);
    let credentials = credentials::snapshot(ctx, account).await?;
    let payload =
        client::token_request(&client, "GET", RECORDS_PATH, &credentials.session, None, "积分查询")
            .await?;

    let code = client::business_code(&payload);
    if client::is_auth_error_code(&code) {
        return Err("Loomy 登录态已失效，无法查询积分（请重新登录）".to_string());
    }
    if code != client::SUCCESS_CODE {
        let message = client::upstream_message(&payload);
        return Err(if message.trim().is_empty() {
            format!("积分查询失败（上游 code={code}）")
        } else {
            format!("积分查询失败：{message}")
        });
    }

    // 摘要可能在 data 或 data.data 两层（V2 接口的形态在不同版本里出现过两种）
    let data = payload
        .get("data")
        .and_then(|value| value.get("data"))
        .unwrap_or_else(|| payload.get("data").unwrap_or(&Value::Null));

    let permanent = pick_number(data, "balance").unwrap_or(0.0);
    let daily = pick_number(data, "dailyBalance").unwrap_or(0.0);
    let daily_remaining = pick_number(data, "dailyRemainingPoints");
    let daily_limit = pick_number(data, "dailyLimitPoints");

    let mut wallets = vec![
        json!({
            "type": "permanent_points",
            "displayName": "永久积分",
            "balance": permanent,
        }),
        json!({
            "type": "daily_points",
            "displayName": "每日赠送积分",
            "balance": daily,
        }),
    ];
    // 赠送额度的「剩余 / 上限」是独立的展示口径，有就带上，没有不编造
    if let (Some(remaining), Some(limit)) = (daily_remaining, daily_limit) {
        if let Some(object) = wallets.get_mut(1).and_then(Value::as_object_mut) {
            object.insert("remaining".to_string(), Value::from(remaining));
            object.insert("limit".to_string(), Value::from(limit));
        }
    }

    // 逐份明细（`credits[]`，账号页「积分明细」弹层的输入）：两行与 `wallets`
    // 同序同口径。`daily: true` 是界面「每日重置」的判据 —— 余额读到 0 时也不
    // 折进「已用完」（明天回满）。
    //
    // `dailyRemainingPoints` / `dailyLimitPoints` **不折进这两行**：它们是赠送
    // 额度的另一个展示口径，与 `dailyBalance` 是否同源未经证实 —— 混进来的
    // 「剩 x / 共 y」可能对不上真正的余额（宁可少给，不编）。
    let credits = vec![
        json!({ "name": "永久积分", "remain": permanent }),
        json!({ "name": "每日赠送积分", "remain": daily, "daily": true }),
    ];

    let mut document = json!({
        "available": permanent + daily,
        "unit": "积分",
        "wallets": wallets,
        "raw": payload,
    });
    if let Some(object) = document.as_object_mut() {
        object.insert("credits".to_string(), Value::Array(credits));
    }
    Ok(document)
}
