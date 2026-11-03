//! 额度查询：归一成账号页的余额形态（`available` / `unit` / `wallets` / `credits`）。
//!
//! ── Qoder 的三个桶（2026-09-27 在真实账号上实测）──────────────────
//!   - `userQuota`：订阅套餐的额度（官方控制台「订阅版本的资源」，如总额 300、
//!     剩余 225）。`available` 的主口径，字段 `total/used/remaining/unit`；
//!   - `addOnQuota`：官方控制台叫「**个人资源包**」——通过活动/购买获得，
//!     每日签到每签一次发一个 100 credits、30 天有效的包（领 6 次就是 600）。
//!     **上游只在真有资源包时才返回这个键**：没有就整个键缺失（实测 Free 套餐
//!     账号如此），因此要判「有没有」，不能当成恒在的桶去读零；
//!   - `orgResourcePackage`：组织（团队）资源包。**字段形状与上面两个都不同**：
//!     总量叫 `cap`（不是 `total`）、另外多一个 `available` 布尔标它是否真的可用。
//!
//! ── 千问办公读的是另一份文档 ────────────────────────────────
//! 它的账号上下文（`/api/v1/adapter/user/account-context`）里给
//! `plan`（套餐名 + 下次刷新日）与 `quota`（总量 / 已用 / 剩余），
//! 公有云的个人账号另外有 `/api/v1/adapter/user/wallets` 三档积分池。
//! **个人账号的 `quota.total` 与 `used` 是 null**，只有 `remaining` 是真实的
//! —— 所以那时只报余额、不编造「已用占比」（与它自己的客户端一致）。

use serde_json::{json, Map, Value};

use agent2api_provider_protocol::WireProxy as ResolvedProxy;
use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;

use super::credentials::Credentials;
use super::endpoints::{self, Site};
use super::{auth, refresh};

/// `expiresAt` 的「永不过期」哨兵值：上游给 `253402214400000`（9999-12-31）。
///
/// 原样透出会让余额面板显示「到期 9999/12/31」——那不是用户能用的信息。
/// 判据取 2100-01-01：真实订阅（含最长的年付）不可能超过它。
const EXPIRY_SENTINEL_MS: f64 = 4_102_444_800_000.0;

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(value) => value.as_f64(),
        Value::String(value) => value.parse::<f64>().ok().filter(|value| value.is_finite()),
        _ => None,
    }
}

/// 一行钱包（账号页余额面板按 `displayName` + `balanceView` 渲染）。
fn wallet(kind: &str, name: &str, value: f64, unit: &str) -> Value {
    json!({
        "type": kind,
        "displayName": name,
        "balance": value,
        "balanceView": format!("{value} {unit}"),
    })
}

/// `addOnQuota` 桶本身（键在、是对象、且**不是空桶**）。
///
/// 判据只有这一份（「键缺失或全零 = 没有资源包」，见模块头）：[`add_on`] 的钱包行
/// 与 [`credit_rows`] 的个人资源包行都从这里取桶 —— 两处各判一次迟早漂成两种口径。
fn add_on_bucket(raw: &Value) -> Option<&Value> {
    let bucket = raw.get("addOnQuota").filter(|value| value.is_object())?;
    let total = number(bucket.get("total"));
    let used = number(bucket.get("used"));
    let remaining = number(bucket.get("remaining"));
    if [total, used, remaining].iter().all(|value| value.unwrap_or(0.0) <= 0.0) {
        return None;
    }
    Some(bucket)
}

/// 个人资源包（`addOnQuota`）→ 钱包行 + 剩余额度。
///
/// 没有这个键、或全零（上游会返回空对象）时给空结果 —— 与参考实现的判据一致：
/// 「全零」意味着没有资源包，而不是「有 0 个」（判据本体在 [`add_on_bucket`]）。
fn add_on(raw: &Value, fallback_unit: &str) -> (Vec<Value>, Option<f64>) {
    let Some(bucket) = add_on_bucket(raw) else {
        return (Vec::new(), None);
    };
    let total = number(bucket.get("total"));
    let used = number(bucket.get("used"));
    let remaining = number(bucket.get("remaining"));
    let unit = bucket.get("unit").and_then(Value::as_str).unwrap_or(fallback_unit);
    let mut wallets = Vec::new();
    if let Some(value) = used {
        wallets.push(wallet("addon_used", "资源包已用", value, unit));
    }
    if let Some(value) = total {
        wallets.push(wallet("addon_total", "资源包总额", value, unit));
    }
    (wallets, remaining)
}

/// `orgResourcePackage` 桶本身（键在、是对象、且上游说它**可用**）。
fn org_bucket(raw: &Value) -> Option<&Value> {
    let bucket = raw.get("orgResourcePackage").filter(|value| value.is_object())?;
    let cap = number(bucket.get("cap")).or_else(|| number(bucket.get("total")));
    let used = number(bucket.get("used"));
    let remaining = number(bucket.get("remaining"));
    let usable = bucket.get("available").and_then(Value::as_bool).unwrap_or_else(|| {
        [cap, used, remaining].iter().any(|value| value.unwrap_or(0.0) > 0.0)
    });
    if usable { Some(bucket) } else { None }
}

/// 组织资源包（`orgResourcePackage`）→ 钱包行 + 剩余额度。
///
/// 判据与上面两桶**都不同**（见模块头）：
///   * 总量字段叫 `cap`（旧形状里叫 `total`，作兜底读）；
///   * 空桶也**整个键返回**，所以「键在不在」不能当判据；上游用 `available`
///     表明这份额度当前是否真的可用，`false` 时整桶跳过；
///   * `remaining` 只在可用时算进「还能花多少」。
fn org_package(raw: &Value, fallback_unit: &str) -> (Vec<Value>, Option<f64>) {
    let Some(bucket) = org_bucket(raw) else {
        return (Vec::new(), None);
    };
    let cap = number(bucket.get("cap")).or_else(|| number(bucket.get("total")));
    let used = number(bucket.get("used"));
    let remaining = number(bucket.get("remaining"));
    let unit = bucket.get("unit").and_then(Value::as_str).unwrap_or(fallback_unit);
    let mut wallets = Vec::new();
    if let Some(total) = cap.filter(|value| *value > 0.0) {
        wallets.push(wallet("org_total", "组织总额", total, unit));
    }
    if let Some(value) = used {
        wallets.push(wallet("org_used", "组织已用", value, unit));
    }
    (wallets, remaining)
}

/// 逐桶明细行（账号页「积分明细」弹层的输入；行形状与 workbuddy / trae 两家
/// 同一套 —— 界面只写一个读取器）。
///
/// 三个数直接透传桶上的 `total` / `used` / `remaining`，**缺失不写键**
/// （界面 `Number(null) === 0`，写 null 会把「没读到」画成「只剩 0」）。
fn credit_rows(raw: &Value, doc_unit: &str) -> Vec<Value> {
    let mut rows: Vec<Value> = Vec::new();
    if let Some(bucket) = raw.get("userQuota").filter(|value| value.is_object()) {
        if let Some(row) = credit_row(
            "套餐额度",
            number(bucket.get("total")),
            number(bucket.get("used")),
            number(bucket.get("remaining")),
            bucket.get("unit").and_then(Value::as_str),
            doc_unit,
        ) {
            rows.push(row);
        }
    }
    if let Some(bucket) = add_on_bucket(raw) {
        if let Some(row) = credit_row(
            "个人资源包",
            number(bucket.get("total")),
            number(bucket.get("used")),
            number(bucket.get("remaining")),
            bucket.get("unit").and_then(Value::as_str),
            doc_unit,
        ) {
            rows.push(row);
        }
    }
    if let Some(bucket) = org_bucket(raw) {
        if let Some(row) = credit_row(
            "组织资源包",
            number(bucket.get("cap")).or_else(|| number(bucket.get("total"))),
            number(bucket.get("used")),
            number(bucket.get("remaining")),
            bucket.get("unit").and_then(Value::as_str),
            doc_unit,
        ) {
            rows.push(row);
        }
    }
    rows
}

/// 一行明细：`name` + 三个数 + （与文档级不同的）单位；三个数一个都没有时给
/// `None`（整行不出，理由见 [`credit_rows`]）。
fn credit_row(
    name: &str,
    total: Option<f64>,
    used: Option<f64>,
    remain: Option<f64>,
    unit: Option<&str>,
    doc_unit: &str,
) -> Option<Value> {
    if total.is_none() && used.is_none() && remain.is_none() {
        return None;
    }
    let mut row = Map::new();
    row.insert("name".to_string(), Value::String(name.to_string()));
    for (key, value) in [("total", total), ("used", used), ("remain", remain)] {
        if let Some(value) = value {
            row.insert(key.to_string(), json!(value));
        }
    }
    if let Some(unit) = unit.map(str::trim).filter(|value| !value.is_empty() && *value != doc_unit) {
        row.insert("unit".to_string(), Value::String(unit.to_string()));
    }
    Some(Value::Object(row))
}

/// 订阅到期时间；哨兵值、0 与 null 都按「没有到期时间」处理（不显示）。
fn expiry_of(raw: &Value) -> Option<Value> {
    let value = raw.get("expiresAt")?;
    if value.is_null() {
        return None;
    }
    if let Some(ms) = number(Some(value)) {
        if ms <= 0.0 || ms >= EXPIRY_SENTINEL_MS {
            return None;
        }
    }
    Some(value.clone())
}

/// 套餐名（`plan_tier_name`）。**best-effort**：失败只让这一行不显示。
async fn plan_name(credentials: &Credentials, proxy: Option<&ResolvedProxy>) -> Option<String> {
    let response = auth::request(
        "GET",
        &format!("{}{}", credentials.site.open_api(), endpoints::PLAN_PATH),
        None,
        &auth::account_headers(credentials.site, Some(&credentials.access_token)),
        proxy,
    ).await.ok()?;
    let name = response.payload?.get("plan_tier_name")?.as_str()?.trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// 额度文档的纯函数半边（Qoder）：三个桶合成钱包明细、`available` 与逐桶明细。
///
/// 抽出来是为了让「组织资源包计不计入 `available`」这类判据能单测钉住——
/// 这一带的历史事故全是「明明读到了却显示成没有」，而症状在界面上。
fn usage_document(raw: &Value) -> Value {
    let quota = raw.get("userQuota");
    let remaining = quota.and_then(|quota| number(quota.get("remaining")));
    let total = quota.and_then(|quota| number(quota.get("total")));
    let used = quota.and_then(|quota| number(quota.get("used")));
    let unit = quota.and_then(|quota| quota.get("unit")).and_then(Value::as_str).unwrap_or("额度");
    let mut wallets = Vec::new();
    if let Some(value) = used {
        wallets.push(json!({ "type": "user_used", "displayName": "个人已用", "balance": value }));
    }
    if let Some(value) = total {
        wallets.push(json!({ "type": "user_total", "displayName": "个人总额", "balance": value }));
    }
    let (add_on_wallets, add_on_remaining) = add_on(raw, unit);
    wallets.extend(add_on_wallets);
    let (org_wallets, org_remaining) = org_package(raw, unit);
    wallets.extend(org_wallets);
    // 「还能花多少」= 套餐剩余 + 个人资源包剩余 + 组织资源包剩余（都能花，见模块头）
    let available = [remaining, add_on_remaining, org_remaining]
        .into_iter()
        .flatten()
        .reduce(|acc, value| acc + value);
    let credits = credit_rows(raw, unit);
    let mut document = json!({
        "available": available,
        "unit": unit,
        "wallets": wallets,
    });
    if !credits.is_empty() {
        if let Some(object) = document.as_object_mut() {
            object.insert("credits".to_string(), Value::Array(credits));
        }
    }
    document
}

// ─── 千问办公 ───────────────────────────────────────────────────

/// 账号上下文的 `data` 那一层（客户端自己的 `unwrapAdapterResponse`：
/// 有 `data` 对象就用它，否则用信封本身）。
fn unwrap(value: &Value) -> &Value {
    value
        .get("data")
        .filter(|data| data.is_object())
        .unwrap_or(value)
}

/// 三档积分池的 key 与展示名（顺序照客户端：每日 / 每月 / 长期）。
const POOLS: &[(&str, &str, &str)] = &[
    ("daily_credits", "每日积分", "daily_credits"),
    ("monthly_credits", "每月积分", "monthly_credits"),
    ("longterm_credits", "长期积分", "longterm_credits"),
];

/// 钱包（`/api/v1/adapter/user/wallets`）→ 逐格余额。
///
/// 每一格的 `total_balance` 是这一档**还剩多少**（不是总额）；读不到的池不
/// 出行（不编造 0）。
fn wallet_pools(wallets: &Value) -> Vec<(String, f64)> {
    let data = unwrap(wallets);
    let mut out = Vec::new();
    for (key, label, _) in POOLS {
        let pool = data.get(*key);
        let balance = pool
            .and_then(|pool| number(pool.get("total_balance")))
            .or_else(|| pool.and_then(|pool| number(pool.get("totalBalance"))));
        if let Some(balance) = balance.filter(|value| *value > 0.0) {
            out.push((label.to_string(), balance));
        }
    }
    out
}

/// 账号上下文 + 钱包 → 账号页的余额文档（千问办公）。
///
/// ── 为什么 `total` / `used` 可能是空 ────────────────────────
/// 个人账号的 `quota` 只给 `remaining`（`total` 与 `used` 都是 null）——
/// 那时只报余额，不反推出一个「已用占比」（那正是「编造一个 0 总额」的来源）。
/// 企业账号或有总额的账号照常给「已用 / 总额」那一行。
fn qwenwork_document(context: &Value, wallets: Option<&Value>) -> Value {
    let data = unwrap(context);
    let plan = data.get("plan").filter(|value| value.is_object());
    let quota = data.get("quota").filter(|value| value.is_object());

    let mut document = Map::new();
    let mut unit = "credits";
    let mut credits: Vec<Value> = Vec::new();
    let mut available: Option<f64> = None;

    if let Some(quota) = quota {
        let total = number(quota.get("total"));
        let remaining = number(quota.get("remaining"));
        let used = number(quota.get("used")).or_else(|| match (total, remaining) {
            (Some(total), Some(remaining)) => Some(total - remaining),
            _ => None,
        });
        if let Some(row) = credit_row("积分", total, used, remaining, None, unit) {
            credits.push(row);
        }
        available = remaining;
    }
    // 公有云的个人账号有这三档池：它们的分项是「余额里都是什么」，
    // 所以只作明细行，不再叠加进 `available`（那份余额本来就是三档之和）。
    if let Some(wallets) = wallets {
        for (label, balance) in wallet_pools(wallets) {
            if let Some(row) = credit_row(&label, None, None, Some(balance), None, unit) {
                credits.push(row);
            }
        }
    }
    if unit.is_empty() {
        unit = "credits";
    }
    document.insert("available".to_string(), available.map_or(Value::Null, |value| json!(value)));
    document.insert("unit".to_string(), Value::String(unit.to_string()));
    document.insert("wallets".to_string(), Value::Array(Vec::new()));
    if !credits.is_empty() {
        document.insert("credits".to_string(), Value::Array(credits));
    }
    let mut subscription = Map::new();
    if let Some(name) = plan.and_then(|plan| plan.get("name")).and_then(Value::as_str) {
        if !name.trim().is_empty() {
            subscription.insert("planName".to_string(), Value::String(name.trim().to_string()));
        }
    }
    let renew = plan
        .and_then(|plan| plan.get("next_refresh_date").or_else(|| plan.get("next_due_date")))
        .and_then(Value::as_str)
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map(|time| time.timestamp_millis());
    if let Some(renew) = renew {
        subscription.insert("expireAt".to_string(), json!(renew));
    }
    document.insert("subscription".to_string(), Value::Object(subscription));
    Value::Object(document)
}

/// 读一份账号接口（带令牌），可指定路径。
async fn read(site: Site, path: &str, token: &str, proxy: Option<&ResolvedProxy>) -> Result<Value, GatewayError> {
    let response = auth::request(
        "GET",
        &format!("{}{}", site.open_api(), path),
        None,
        &auth::account_headers(site, Some(token)),
        proxy,
    ).await?;
    if !response.ok {
        return Err(GatewayError::with_status(
            i32::from(response.status),
            format!("额度查询失败（HTTP {}）", response.status),
        ));
    }
    response
        .payload
        .ok_or_else(|| GatewayError::with_status(502, "额度查询未返回有效 JSON"))
}

pub async fn query(
    ctx: &PluginCtx,
    site: Site,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let mut credentials = refresh::ensure_fresh(ctx, site, account_id, false).await?;
    let (record, _) = refresh::snapshot(ctx, site, account_id).await?;
    let proxy = auth::account_proxy(&record)?;
    if site.is_qwenwork() {
        return qwenwork_usage(ctx, site, account_id, &mut credentials, proxy.as_ref()).await;
    }
    // ── Qoder ──────────────────────────────────────────────────
    let usage_path = endpoints::USAGE_PATH;
    let mut response = auth::request(
        "GET",
        &format!("{}{}", credentials.site.open_api(), usage_path),
        None,
        &auth::account_headers(credentials.site, Some(&credentials.access_token)),
        proxy.as_ref(),
    ).await?;
    if response.status == 401 && credentials.can_refresh() {
        credentials = refresh::ensure_fresh(ctx, site, account_id, true).await?;
        response = auth::request(
            "GET",
            &format!("{}{}", credentials.site.open_api(), usage_path),
            None,
            &auth::account_headers(credentials.site, Some(&credentials.access_token)),
            proxy.as_ref(),
        ).await?;
    }
    let raw = auth::payload(response, "额度查询")?;
    let mut document = usage_document(&raw);
    let mut subscription = Map::new();
    if let Some(name) = plan_name(&credentials, proxy.as_ref()).await {
        subscription.insert("planName".to_string(), Value::String(name));
    }
    if let Some(value) = expiry_of(&raw) {
        subscription.insert("expireAt".to_string(), value);
    }
    if let Some(object) = document.as_object_mut() {
        object.insert("subscription".to_string(), Value::Object(subscription));
    }
    Ok(document)
}

/// 千问办公的余额：账号上下文 + （个人账号才有）三档钱包。
///
/// 401 与 Qoder 同一条处置：**强制**续期凭证一次再读一遍（不看的临期窗口，
/// 因为「时间上还很新但被服务端作废」的令牌正是这一档要救的）。
async fn qwenwork_usage(
    ctx: &PluginCtx,
    site: Site,
    account_id: &str,
    credentials: &mut Credentials,
    proxy: Option<&ResolvedProxy>,
) -> Result<Value, GatewayError> {
    let usage_path = site
        .usage_path()
        .ok_or_else(|| GatewayError::with_status(500, "这家没有配置额度读取路径"))?;
    let mut context = read(site, usage_path, &credentials.access_token, proxy).await;
    if matches!(&context, Err(error) if error.status_code == 401 || error.status_code == 403)
        && credentials.can_refresh()
    {
        *credentials = refresh::ensure_fresh(ctx, site, account_id, true).await?;
        context = read(site, usage_path, &credentials.access_token, proxy).await;
    }
    let context = context?;

    // 钱包：只有国内版的**个人**账号有（`is_biz` 为假）。读失败不阻断 ——
    // 余额本身已经在手，分项读不到就只少明细行。
    let mut wallets: Option<Value> = None;
    if let Some(wallet_path) = site.wallets_path() {
        let user = unwrap(&context)
            .get("user")
            .filter(|value| value.is_object())
            .cloned()
            .unwrap_or(Value::Null);
        let is_biz = user.get("is_biz").or_else(|| user.get("isBiz"));
        if is_biz.and_then(Value::as_bool) != Some(true) {
            wallets = read(site, wallet_path, &credentials.access_token, proxy).await.ok();
        }
    }
    Ok(qwenwork_document(&context, wallets.as_ref()))
}
