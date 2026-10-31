//! Qoder 个人额度、个人资源包与组织资源包，统一为账号页的余额形态。
//!
//! ── 三个桶分别是什么（2026-09-27 在真实账号上实测）──────────────
//!   - `userQuota`：订阅套餐的额度（官方控制台「订阅版本的资源」，如总额 300、
//!     剩余 225）。`available` 的主口径，字段 `total/used/remaining/unit`；
//!   - `addOnQuota`：官方控制台叫「**个人资源包**」——通过活动/购买获得，
//!     每日签到每签一次发一个 100 credits、30 天有效的包（领 6 次就是 600）。
//!     **上游只在真有资源包时才返回这个键**：没有就整个键缺失（实测 Free 套餐
//!     账号如此），因此要判「有没有」，不能当成恒在的桶去读零；
//!   - `orgResourcePackage`：组织（团队）资源包。**字段形状与上面两个都不同**：
//!     总量叫 `cap`（不是 `total`）、另外多一个 `available` 布尔标它是否真的可用。
//!     实测形状（2026-10-12，国际版 Teams 账号）：
//!     `{"used":0,"remaining":0,"percentage":0,"unit":"credits","cap":0,"available":false}`
//!     ——空桶也会**整个键返回**，所以「键在不在」不能当判据；要看 `available`
//!     与 `cap`，否则会把一个还没发放的组织包画成「组织总额 0」。
//!
//! 漏掉 `addOnQuota` 正是 issue #26 的直接原因：签到领到的 600 credits 全在那个桶里，
//! 而这里此前只读 `userQuota` 与 `orgResourcePackage`，界面上一个数字都不显示。
//! 三个独立实现都是同一字段名（CLIProxyAPI 的 qoder2api 插件标「个人拓展包」、
//! CPA 的 qoder 插件、10router 的 CHANGELOG 里那条同样的修复）。
//!
//! ── `available` 为什么把资源包算进去 ────────────────────────
//! 账号页那一列回答的是「这个账号还能花多少」，而资源包里的 credits 与套餐额度
//! 一样能花（官方控制台把两者并列显示）。所以
//! `available = userQuota.remaining + addOnQuota.remaining + orgResourcePackage.remaining`
//! （组织包那一项仅在 `available:true` 时计入），钱包明细里再拆开给用户看——
//! 相加之后「签到领了 100」会立刻反映在列表上，不用展开面板才发现。
//!
//! ── 套餐名与到期 ───────────────────────────────────────────
//! 套餐名来自 `GET /api/v2/user/plan` 的 `plan_tier_name`（`Free` / `Pro Trial`），
//! 取不到就不显示（额度接口才是主链路，套餐名只是补一行）。到期的**哨兵值**见
//! [`EXPIRY_SENTINEL_MS`]。
//!
//! 逐份资源包的到期时间不在 openapi 的聚合桶里：配置网页 Cookie 后，本模块
//! 额外请求 `BIG_MODEL_CREDITS_PATH`，把 `resource_package_quota.quota_detail`
//! 的 `expires_at` 写成 `credits[].expireAt`。没有 Cookie 时只出聚合行，
//! 「积分到期时间」排序显示暂无数据，不影响原有余额读数。

use serde_json::{json, Map, Value};

use agent2api_provider_sdk::PluginCtx;
use agent2api_provider_protocol::WireProxy as ResolvedProxy;
use crate::errors::GatewayError;
use crate::logging;

use super::credentials::{self, Credentials};
use super::{auth, endpoints, refresh};

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

/// 时间戳归一成毫秒（网页接口给毫秒；防御性兼容秒）。
fn millis(value: Option<&Value>) -> Option<i64> {
    let value = number(value)?;
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    Some(if value < 100_000_000_000.0 {
        (value * 1000.0) as i64
    } else {
        value as i64
    })
}

fn source_label(source: &str) -> &'static str {
    match source {
        "RESOURCE_PACKAGE_SOURCE_BONUS" => "活动赠送",
        "RESOURCE_PACKAGE_SOURCE_PURCHASE" => "购买资源包",
        "PLAN" => "套餐额度",
        _ => "资源包",
    }
}

fn package_name(source: &str, id: &str) -> String {
    let label = source_label(source);
    let short: String = id.chars().take(8).collect();
    if short.is_empty() {
        label.to_string()
    } else {
        format!("{label} · {short}")
    }
}

/// 网页积分明细 → 逐份资源包行（主仓库「积分到期时间」排序的输入）。
///
/// 只读 `resource_package_quota.quota_detail`：`total_quota.quota_detail` 是
/// 套餐 + 资源包的重复汇总，读它会重复出行。跳过未启用、已用完、没有到期
/// 时间的包；`expireAt` 统一为毫秒。名字带资源包 id 前 8 位，便于同源码的
/// 多份包在界面上区分。
pub(crate) fn package_rows(raw: &Value, doc_unit: &str) -> Vec<Value> {
    let Some(detail) = raw
        .get("resource_package_quota")
        .and_then(|quota| quota.get("quota_detail"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut rows: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for pack in detail {
        if pack
            .get("is_active")
            .and_then(Value::as_bool)
            == Some(false)
        {
            continue;
        }
        if let Some(status) = pack.get("status").and_then(Value::as_str) {
            if !status.eq_ignore_ascii_case("ACTIVE") {
                continue;
            }
        }
        let remain = number(pack.get("remaining_value"));
        if !remain.is_some_and(|value| value > 0.0) {
            continue;
        }
        let Some(expire_at) = millis(pack.get("expires_at")) else {
            continue;
        };
        let id = pack.get("id").and_then(Value::as_str).unwrap_or("");
        if !id.is_empty() && !seen.insert(id.to_string()) {
            continue;
        }
        let source = pack.get("source").and_then(Value::as_str).unwrap_or("");
        let mut row = Map::new();
        row.insert("name".to_string(), Value::String(package_name(source, id)));
        if let Some(value) = number(pack.get("limit_value")) {
            row.insert("total".to_string(), json!(value));
        }
        if let Some(value) = number(pack.get("used_value")) {
            row.insert("used".to_string(), json!(value));
        }
        row.insert("remain".to_string(), json!(remain.unwrap_or(0.0)));
        row.insert("expireAt".to_string(), Value::from(expire_at));
        if !id.is_empty() {
            row.insert("id".to_string(), Value::String(id.to_string()));
        }
        if let Some(unit) = pack
            .get("unit")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty() && *value != doc_unit)
        {
            row.insert("unit".to_string(), Value::String(unit.to_string()));
        }
        rows.push(Value::Object(row));
    }
    rows.sort_by_key(|row| {
        row.get("expireAt")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MAX)
    });
    rows
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
///
/// 判据细则见 [`org_package`] 的文档；这里与 [`add_on_bucket`] 同一取向 ——
/// 判据只写一份，钱包行与 [`credit_rows`] 的组织资源包行都从这里取桶。
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
///   * 总量字段叫 `cap`（旧形状里叫 `total`，作兜底读）—— 此前只按 `total` 读，
///     而新响应里没有这个键，读到的恒是 `None`，组织额度永远不会出现在面板上；
///   * 空桶也**整个键返回**，所以「键在不在」不能当判据；上游用 `available`
///     表明这份额度当前是否真的可用，`false` 时整桶跳过，免得把还没发放的
///     组织包画成「组织总额 0」；
///   * `remaining` 只在可用时返回 —— 不可用的包即使带着非零 `remaining`
///     也不能算进「还能花多少」。
///
/// `available` 缺失（旧形状）时退回老判据：`cap` / `used` / `remaining`
/// 有一个大于零就显示。判据本体在 [`org_bucket`]。
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
/// 一行一个桶：套餐额度 → 个人资源包 → 组织资源包（与 `wallets` 的 push 顺序
/// 一致）。三个数直接透传桶上的 `total` / `used` / `remaining`，**缺失不写键**
/// （界面 `Number(null) === 0`，写 null 会把「没读到」画成「只剩 0」）。
///
/// ── 出行的判据全部与钱包行同源 ──────────────────────────────
///   * 套餐行：三个数一个都读不到时整行不出 —— 行里没有数字可读，界面只能画
///     「—」（与 ZCode 那边「纯比例窗口行跳过」同一条纪律）；一个桶都没读到
///     时 `credits` 键整个不写，界面按「键在不在」自然退回原样。
///   * 个人资源包 / 组织资源包两行：判据取 [`add_on_bucket`] / [`org_bucket`]
///     —— 「有没有这份额度」只有一份判据，见那两个函数。
///
/// ── 时刻与单位 ──────────────────────────────────────────────
///   * 聚合行不写 `expireAt` / `grantAt`：桶级没有这些字段；`subscription.expireAt`
///     是**订阅**到期，与「这一份额度什么时候作废」是两回事，不拿它顶替；
///   * 逐份资源包行由 [`package_rows`] 写入网页接口给的 `expires_at`（毫秒），
///     这样主仓库才能按「最快到期优先」排序；
///   * `unit` 只在该桶自己的单位非空且**与文档级不同**时写（文档级取自
///     `userQuota.unit`，缺省「额度」；三桶通常同单位，多数情况都不写）。
fn credit_rows(raw: &Value, doc_unit: &str, packages: &[Value]) -> Vec<Value> {
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
    if packages.is_empty() {
        // 没有逐份明细（未配 Cookie / 接口失败）时保留聚合行，界面至少还能看到
        // 「个人资源包」总额；有逐份明细时用明细替换这一行，避免重复。
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
    } else {
        rows.extend_from_slice(packages);
    }
    if let Some(bucket) = org_bucket(raw) {
        if let Some(row) = credit_row(
            "组织资源包",
            // 总量字段叫 `cap`（旧形状叫 `total`，与钱包行同一兜底，见 [`org_package`]）
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
    // 上游给什么写什么（读到 0 也照写：那是真读数，不是「没读到」）；
    // 缺失一律不写这个键
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

/// 套餐名（`plan_tier_name`）。**best-effort**：失败只让这一行不显示，
/// 不影响额度本身（额度接口已经成功，套餐名只是补充信息）。
async fn plan_name(credentials: &Credentials, proxy: Option<&ResolvedProxy>) -> Option<String> {
    let response = auth::request(
        "GET",
        &format!("{}{}", credentials.region.open_api(), endpoints::PLAN_PATH),
        None,
        &endpoints::open_api_headers(Some(&credentials.access_token)),
        proxy,
    ).await.ok()?;
    let name = response.payload?.get("plan_tier_name")?.as_str()?.trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// 额度文档的纯函数半边：三个桶合成钱包明细、`available` 与顶层逐桶明细
/// （`credits`，见 [`credit_rows`]）。
///
/// 抽出来是为了让「组织资源包计不计入 `available`」这类判据能单测钉住——
/// 这一带的历史事故全是「明明读到了却显示成没有」，而症状在界面上，
/// 光看 `query` 的返回值很难回归。
fn usage_document(raw: &Value, packages: &[Value]) -> Value {
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
    let credits = credit_rows(raw, unit, packages);
    let mut document = json!({
        "available": available,
        "unit": unit,
        "wallets": wallets,
    });
    // 逐桶明细挂文档顶层（见 [`credit_rows`]）；空数组**整键不写** —— 界面按
    // 「键在不在 + 长度」决定挂不挂「积分明细」入口，写一个空数组会挂出一个
    // 点开是空的入口。
    if !credits.is_empty() {
        if let Some(object) = document.as_object_mut() {
            object.insert("credits".to_string(), Value::Array(credits));
        }
    }
    document
}

/// 用网页会话 Cookie 读逐份积分包；Cookie 缺失、失效或属于别的账号时返回错误。
///
/// 这个接口是**增强**：调用方在余额查询里要吞掉错误、回落到聚合读数；只有
/// 管理动作 `webcookie.set` 才把错误直接交给用户。
pub(crate) async fn big_model_credits(
    credentials: &Credentials,
    proxy: Option<&ResolvedProxy>,
) -> Result<Vec<Value>, GatewayError> {
    if credentials.web_cookie.is_empty() {
        return Err(GatewayError::with_status(400, "还没有配置 Qoder 网页 Cookie"));
    }
    let response = auth::request(
        "GET",
        &format!(
            "{}{}",
            credentials.region.web_origin(),
            endpoints::BIG_MODEL_CREDITS_PATH
        ),
        None,
        &endpoints::web_cookie_headers(credentials.region, &credentials.web_cookie),
        proxy,
    )
    .await?;
    let raw = auth::payload(response, "积分明细查询")?;
    let cookie_user = raw.get("user_id").and_then(Value::as_str).unwrap_or("");
    if !cookie_user.is_empty()
        && !credentials.user_id.is_empty()
        && cookie_user != credentials.user_id
    {
        return Err(GatewayError::with_status(
            400,
            "这枚网页 Cookie 属于另一个 Qoder 账号，请登录当前账号后重试",
        ));
    }
    Ok(package_rows(&raw, "credits"))
}

pub(crate) async fn web_cookie_info(
    ctx: &PluginCtx,
    region: super::endpoints::Region,
    account: &str,
) -> Result<Value, GatewayError> {
    if account.trim().is_empty() {
        return Err(GatewayError::with_status(400, "Qoder 网页 Cookie 需要指定账号"));
    }
    let record = crate::store::record(ctx, region.provider_id(), account).await?;
    let configured = !crate::store::pick(&record, &["webCookie", "web_cookie"]).is_empty();
    Ok(json!({ "configured": configured }))
}

pub(crate) async fn web_cookie_set(
    ctx: &PluginCtx,
    region: super::endpoints::Region,
    account: &str,
    payload: &Value,
) -> Result<Value, GatewayError> {
    if account.trim().is_empty() {
        return Err(GatewayError::with_status(400, "Qoder 网页 Cookie 需要指定账号"));
    }
    let cookie = credentials::cookie(payload)?;
    if cookie.is_empty() {
        return Err(GatewayError::with_status(400, "请提供 Qoder 网页 Cookie"));
    }
    let record = crate::store::record(ctx, region.provider_id(), account).await?;
    let mut credentials = Credentials::from_payload(&record)?;
    credentials.region = region;
    credentials.web_cookie = cookie.clone();
    if credentials.user_id.is_empty() {
        return Err(GatewayError::with_status(400, "Qoder 账号缺少 userId，无法校验 Cookie"));
    }
    let proxy = auth::account_proxy(&record)?;
    // 先真正打一次积分明细：既验证 Cookie 有效，也验证它属于当前账号。
    let packages = big_model_credits(&credentials, proxy.as_ref()).await?;
    let expect = crate::store::secret_view(&record);
    let next = crate::store::with_fields(&expect, &[("webCookie", Value::String(cookie))]);
    let written = crate::store::save(ctx, account, &expect, &next).await?;
    if !written {
        return Err(GatewayError::with_status(
            502,
            "账号凭证刚被其他操作更新，请重新读取后再试",
        ));
    }
    let earliest = packages
        .iter()
        .filter_map(|row| row.get("expireAt").and_then(Value::as_i64))
        .min();
    Ok(json!({
        "configured": true,
        "packages": packages.len(),
        "earliestExpireAt": earliest,
    }))
}

pub(crate) async fn web_cookie_clear(
    ctx: &PluginCtx,
    region: super::endpoints::Region,
    account: &str,
) -> Result<Value, GatewayError> {
    if account.trim().is_empty() {
        return Err(GatewayError::with_status(400, "Qoder 网页 Cookie 需要指定账号"));
    }
    let record = crate::store::record(ctx, region.provider_id(), account).await?;
    let expect = crate::store::secret_view(&record);
    let mut next = expect.clone();
    if let Some(object) = next.as_object_mut() {
        object.remove("webCookie");
        object.remove("web_cookie");
    }
    let written = crate::store::save(ctx, account, &expect, &next).await?;
    if !written {
        return Err(GatewayError::with_status(
            502,
            "账号凭证刚被其他操作更新，请重新读取后再试",
        ));
    }
    Ok(json!({ "configured": false }))
}

pub async fn query(
    ctx: &PluginCtx,
    region: super::endpoints::Region,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let mut credentials = refresh::ensure_fresh(ctx, region, account_id, false).await?;
    let (record, _) = refresh::snapshot(ctx, region, account_id).await?;
    let proxy = auth::account_proxy(&record)?;
    let mut response = auth::request(
        "GET",
        &format!("{}{}", credentials.region.open_api(), endpoints::USAGE_PATH),
        None,
        &endpoints::open_api_headers(Some(&credentials.access_token)),
        proxy.as_ref(),
    ).await?;
    if response.status == 401 && credentials.can_refresh() {
        credentials = refresh::ensure_fresh(ctx, region, account_id, true).await?;
        response = auth::request(
            "GET",
            &format!("{}{}", credentials.region.open_api(), endpoints::USAGE_PATH),
            None,
            &endpoints::open_api_headers(Some(&credentials.access_token)),
            proxy.as_ref(),
        ).await?;
    }
    let raw = auth::payload(response, "额度查询")?;
    let packages = if credentials.web_cookie.is_empty() {
        Vec::new()
    } else {
        match big_model_credits(&credentials, proxy.as_ref()).await {
            Ok(rows) => rows,
            Err(error) => {
                logging::verbose(
                    "[Qoder]",
                    &format!("积分明细查询失败（保留聚合额度）：{}", error.message),
                );
                Vec::new()
            }
        }
    };
    let mut document = usage_document(&raw, &packages);
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


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn package_rows_keeps_active_sorted_packages_only() {
        let raw = json!({
            "resource_package_quota": {
                "quota_detail": [
                    {
                        "id": "01a19030-f27b-7ef1-88da-81e4369703af",
                        "limit_value": 100,
                        "used_value": 0,
                        "remaining_value": 100,
                        "unit": "credits",
                        "expires_at": 1796012488144_i64,
                        "source": "RESOURCE_PACKAGE_SOURCE_BONUS",
                        "status": "ACTIVE",
                        "is_active": true
                    },
                    {
                        "id": "01a171c2-5d1c-7177-9729-7ec54e1a1445",
                        "limit_value": 100,
                        "used_value": 0,
                        "remaining_value": 100,
                        "unit": "credits",
                        "expires_at": 1795501924458_i64,
                        "source": "RESOURCE_PACKAGE_SOURCE_BONUS",
                        "status": "ACTIVE",
                        "is_active": true
                    },
                    {
                        "id": "zero",
                        "remaining_value": 0,
                        "expires_at": 1795501924458_i64,
                        "status": "ACTIVE"
                    },
                    {
                        "id": "expired",
                        "remaining_value": 100,
                        "expires_at": 1795501924458_i64,
                        "status": "EXPIRED"
                    },
                    {
                        "id": "no-expiry",
                        "remaining_value": 100,
                        "expires_at": 0,
                        "status": "ACTIVE"
                    }
                ]
            }
        });
        let rows = package_rows(&raw, "credits");
        assert_eq!(rows.len(), 2, "只保留两份有效包");
        assert_eq!(rows[0]["expireAt"], json!(1795501924458_i64));
        assert_eq!(rows[1]["expireAt"], json!(1796012488144_i64));
        assert_eq!(rows[0]["name"], json!("活动赠送 · 01a171c2"));
        assert_eq!(rows[0]["remain"].as_f64(), Some(100.0));
        assert_eq!(rows[0]["total"].as_f64(), Some(100.0));
    }

    #[test]
    fn usage_document_replaces_addon_row_when_packages_exist() {
        let raw = json!({
            "userQuota": { "total": 300, "used": 75, "remaining": 225, "unit": "credits" },
            "addOnQuota": { "total": 700, "used": 0, "remaining": 700, "unit": "credits" }
        });
        let packages = vec![json!({
            "name": "活动赠送 · 01a171c2",
            "total": 100,
            "used": 0,
            "remain": 100,
            "expireAt": 1795501924458_i64
        })];
        let document = usage_document(&raw, &packages);
        let credits = document.get("credits").and_then(Value::as_array).expect("credits 应存在");
        assert!(credits.iter().any(|row| row.get("name").and_then(Value::as_str) == Some("套餐额度")));
        assert!(!credits.iter().any(|row| row.get("name").and_then(Value::as_str) == Some("个人资源包")));
        assert!(credits.iter().any(|row| row.get("expireAt").and_then(Value::as_i64) == Some(1795501924458)));
    }

    #[test]
    fn usage_document_keeps_addon_row_without_packages() {
        let raw = json!({
            "userQuota": { "total": 300, "used": 75, "remaining": 225, "unit": "credits" },
            "addOnQuota": { "total": 700, "used": 0, "remaining": 700, "unit": "credits" }
        });
        let document = usage_document(&raw, &[]);
        let credits = document.get("credits").and_then(Value::as_array).expect("credits 应存在");
        assert!(credits.iter().any(|row| row.get("name").and_then(Value::as_str) == Some("个人资源包")));
    }
}
