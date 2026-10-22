//! 积分 / 额度查询（对照 Node 版 workbuddy-billing.mjs 的 queryUsage 一族）。
//!
//!   个人账号：POST /v2/billing/meter/get-user-resource   → `query_usage`
//!   企业账号：POST /v2/billing/meter/get-enterprise-user-usage → 同一入口按分支走
//!   组合简报：`credits_summary_from`（纯函数，只给三个数）
//!   逐份明细：`credit_packs_from`（纯函数，账号页弹层用）
//!
//! ── 与网关那份的差别（迁移时改的）──────────────────────────
//! 网关把这一族挂在 `BillingService` 上（那个结构还管签到、活动、套餐等
//! 十来个端点）。插件只需要「查一次积分」，所以这里去掉服务结构，改成
//! 自由函数 + 一个最小的 `call_billing`（见 [`call`]）。
//!
//! 翻译逻辑（上游嵌套结构 → 前端契约）逐字保留 —— 那正是最容易漂移的部分。

use serde_json::{json, Map, Value};

use crate::json::json_number;

use super::commodity::{self, codes, is_bonus, is_daily_credit, is_plan_base, label_of, plan_priority};
use super::tools::{
    js_int_string, js_truthy, number_or_zero, parse_time, time_or_null, to_int, timestamp_json,
};
use super::call;

/// 个人积分包的固定请求体（对照 workbuddy-endpoints.mjs 的 BILLING.userResource.body，
/// ProductCode=p_tcaca 为 WorkBuddy 产品码）。
const USER_RESOURCE_BODY: &str = r#"{"PageNumber":1,"PageSize":100,"ProductCode":"p_tcaca","Status":[0,3],"OnlyValidPeriod":true}"#;

/// 查询账号积分/额度：企业账号走企业接口，个人账号走资源接口。
pub async fn query_usage(
    session: &Value,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    locale: Option<&str>,
) -> Result<Value, crate::errors::GatewayError> {
        let active = session.clone();
        let is_enterprise = active
            .get("account")
            .and_then(|account| account.get("enterpriseId"))
            .map(|value| match value {
                Value::String(text) => !text.is_empty(),
                Value::Null => false,
                other => js_truthy(other),
            })
            .unwrap_or(false);
        if is_enterprise {
            get_enterprise_usage(&active, proxy).await
        } else {
            get_personal_usage(&active, proxy, locale).await
        }
}

/// 个人账号：POST /v2/billing/meter/get-user-resource
async fn get_personal_usage(
    session: &Value,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    locale: Option<&str>,
) -> Result<Value, crate::errors::GatewayError> {
    let body: Value = serde_json::from_str(USER_RESOURCE_BODY).unwrap_or_else(|_| json!({}));
    let result = call::call(
        "POST",
        "/v2/billing/meter/get-user-resource",
        Some(&body),
        session,
        proxy,
        locale,
        call::Options::default(),
    )
    .await?;
        // 结果路径：data.Response.Data.Accounts[]
        let resources: Vec<Value> = result
            .data
            .get("Response")
            .and_then(|value| value.get("Data"))
            .and_then(|value| value.get("Accounts"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut plan_resources: Vec<Value> = resources
            .iter()
            .map(|item| {
                let package_code = item
                    .get("PackageCode")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let is_daily = is_daily_credit(&package_code);
                // Node: `Number(r.CycleCapacitySizePrecise) || 0` —— 非数字/0 都是 0
                let total = number_or_zero(item.get("CycleCapacitySizePrecise"));
                let left = number_or_zero(item.get("CycleCapacityRemainPrecise"));
                // 下面几处都是 Node 的 `parseTime(x) || ...` 形态：parseTime 解析不出
                // 返回 **0**，而 0 在 JS 里是假值 —— 所以「解出 0」与「解不出」等价，
                // `time_or_null` 就是这条语义（0 / 缺失 → None → JSON null）。
                let start_at = time_or_null(item.get("DeductionStartTime"))
                    .or_else(|| time_or_null(item.get("CycleStartTime")));
                let expire_at = time_or_null(if is_daily {
                    item.get("CycleEndTime")
                } else {
                    item.get("DeductionEndTime")
                });
                // Node: `isDaily ? null : (parseTime(r.CycleEndTime) || 0) + 1000 || null`
                // `(0) + 1000 = 1000` 是**真值**，所以非日额度包拿不到 CycleEndTime 时
                // refreshAt 会是 1000 这个哨兵值 —— 照抄（前端据此判断「没有刷新时间」
                // 的行为也照抄）
                let refresh_at = if is_daily {
                    None
                } else {
                    Some(parse_time(item.get("CycleEndTime")) + 1000)
                };
                // ── 领取时刻（`CreateTime`）：与上面两个时间**不是一回事** ────
                // 这份包被发出来的时刻（官方发放记录里的「领取时间」）在这里，
                // 而 `DeductionStartTime` 是开始抵扣、`CycleStartTime` 是本周期的
                // 起点 —— 日额度包三者能差一整天（昨天发出、今天 0 点起算）。
                // 界面那句「领取 …」问的是「这笔什么时候到手的」，所以另存一个
                // 键，不拿 startAt 顶替（口径见 `credit_packs_from`）。
                let create_at = time_or_null(item.get("CreateTime")).map(epoch_to_millis);
                // expireAt 的排序键：缺失视为 Infinity（排在最后）
                let expire_sort = expire_at.map(|value| value as f64).unwrap_or(f64::INFINITY);
                // 名字以**上游原文**为准（`PackageName`），包名码表只在它缺席时
                // 兜底 —— 码表是照抄 Node 版的手抄品，上游改名/换档位后不会跟着
                // 变：issue #239 里 `code_008` 上游叫「CodeBuddy个人体验版」、
                // 周期是月，码表却写着「专业版日额度」，名字与周期都不对。
                let name = item
                    .get("PackageName")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string)
                    .or_else(|| label_of(&package_code).map(str::to_string))
                    .unwrap_or_else(|| package_code.clone());
                // 逐键组装而不是 json! 字面量：Node 里 `id: r.ResourceId` 在字段
                // 缺失时是 undefined，`JSON.stringify` 会**整个丢掉这个键**；
                // json! 的 null 会把它留在响应里，前端按键存在与否判空就会分叉
                let mut resource = Map::new();
                if let Some(resource_id) = item.get("ResourceId") {
                    if !resource_id.is_null() {
                        resource.insert("id".to_string(), resource_id.clone());
                    }
                }
                resource.insert("packageCode".to_string(), Value::String(package_code));
                resource.insert("name".to_string(), Value::String(name));
                resource.insert("isDaily".to_string(), Value::Bool(is_daily));
                // 额度一律写成「整数形态的数」（json_number）：serde_json 把
                // f64 的 100.0 输出成 `100.0`，而 Node 的 JSON.stringify(100)
                // 是 `100` —— 让面板上的「1500 / 1500」与 Node 版逐字一致
                resource.insert("total".to_string(), json_number(total));
                resource.insert("used".to_string(), json_number((total - left).max(0.0)));
                resource.insert("left".to_string(), json_number(left));
                resource.insert("startAt".to_string(), timestamp_json(start_at));
                resource.insert("createAt".to_string(), timestamp_json(create_at));
                resource.insert("expireAt".to_string(), timestamp_json(expire_at));
                resource.insert("refreshAt".to_string(), timestamp_json(refresh_at));
                // 来源一句话（`AccountAttributes` 里的 `grantReason`：官方活动发放 /
                // 拉新奖励 / 每日签到…）。上游没给就**整个不写这个键** —— 写空串
                // 会让界面多出一个没有内容的来源标签。
                if let Some(reason) = grant_reason(item) {
                    resource.insert("grantReason".to_string(), Value::String(reason));
                }
                // 排序辅助字段：排序后会被删掉，不属于契约
                resource.insert("__expireSort".to_string(), json_number(expire_sort));
                resource.insert(
                    "autoRenew".to_string(),
                    Value::Bool(number_or_zero(item.get("AutoRenewFlag")) == 1.0),
                );
                Value::Object(resource)
            })
            .collect();

        // 与桌面端一致：先按套餐优先级，同优先级再按到期时间升序
        plan_resources.sort_by(|a, b| {
            let code_a = a.get("packageCode").and_then(Value::as_str);
            let code_b = b.get("packageCode").and_then(Value::as_str);
            let diff = plan_priority(code_a) - plan_priority(code_b);
            if diff != 0 {
                return diff.cmp(&0);
            }
            let sort_a = a.get("__expireSort").and_then(Value::as_f64).unwrap_or(f64::INFINITY);
            let sort_b = b.get("__expireSort").and_then(Value::as_f64).unwrap_or(f64::INFINITY);
            sort_a.partial_cmp(&sort_b).unwrap_or(std::cmp::Ordering::Equal)
        });
        // 去掉内部排序辅助字段（它不属于契约）
        for item in plan_resources.iter_mut() {
            if let Some(object) = item.as_object_mut() {
                object.remove("__expireSort");
            }
        }

        let mut usage_total = 0.0;
        let mut usage_left = 0.0;
        let mut usage_used = 0.0;
        for item in &plan_resources {
            usage_total += item.get("total").and_then(Value::as_f64).unwrap_or(0.0);
            usage_left += item.get("left").and_then(Value::as_f64).unwrap_or(0.0);
            usage_used += item.get("used").and_then(Value::as_f64).unwrap_or(0.0);
        }

        // 当前生效套餐按等级挑选（旗舰 > 进阶 > 专业 > 青春 > 试用），
        // 而不是简单取列表首个（日额度包金额小但到期最晚，会误判为 free）
        let find_code = |wanted: &[&str]| -> Option<&Value> {
            plan_resources.iter().find(|item| {
                item.get("packageCode")
                    .and_then(Value::as_str)
                    .map(|code| wanted.contains(&code))
                    .unwrap_or(false)
            })
        };
        let flagship = find_code(&[codes::FLAGSHIP]);
        let advanced = find_code(&[codes::ADVANCED]);
        let pro = find_code(&[codes::PRO_YEAR, codes::PRO_MON, codes::PRO_MON_PLUS]);
        let youth = find_code(&[codes::YOUTH]);
        let trial = find_code(&[
            codes::PRO_TRIAL_MON,
            codes::PRO_TRIAL_YEAR,
            codes::FREE_MON_INTL,
            codes::GIFT,
            codes::FREE_MON,
        ]);
        let active = flagship
            .or(advanced)
            .or(pro)
            .or(youth)
            .or(trial)
            .or_else(|| plan_resources.first());

        let edition_type = if flagship.is_some() {
            "flagship"
        } else if advanced.is_some() {
            "advanced"
        } else if youth.is_some() && pro.is_none() {
            "youth"
        } else if pro.is_some() {
            "pro"
        } else if trial.is_some() {
            "pro_trial"
        } else {
            "free"
        };

        Ok(json!({
            "kind": "personal",
            "editionType": edition_type,
            "isPro": pro.is_some(),
            "isTrial": trial.is_some() && pro.is_none() && advanced.is_none()
                && flagship.is_none() && youth.is_none(),
            "planName": active
                .and_then(|item| item.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(""),
            "packageCode": active
                .and_then(|item| item.get("packageCode"))
                .cloned()
                .unwrap_or(Value::Null),
            // 三个合计是**字符串**（Node 用 String(...)）：前端的 `esc()` 直接渲染，
            // 数字形态会让「0」与「0.0」这种差异冒到界面上
            "usageTotal": js_int_string(usage_total),
            "usageLeft": js_int_string(usage_left),
            "usageUsed": js_int_string(usage_used),
            // 这四个都是 `active?.xxx || null` / `active?.xxx ?? false`：active 为
            // null 时也给出 null / false（键始终存在，不是 undefined）
            "expireAt": active
                .and_then(|item| item.get("expireAt"))
                .cloned()
                .filter(|value| !value.is_null())
                .unwrap_or(Value::Null),
            "refreshAt": active
                .and_then(|item| item.get("refreshAt"))
                .cloned()
                .filter(|value| !value.is_null())
                .unwrap_or(Value::Null),
            "autoRenew": active
                .and_then(|item| item.get("autoRenew"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        "resources": plan_resources,
        "raw": result.data,
    }))
}

/// 企业账号：POST /v2/billing/meter/get-enterprise-user-usage
async fn get_enterprise_usage(
    session: &Value,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
) -> Result<Value, crate::errors::GatewayError> {
    let result = call::call(
        "POST",
        "/v2/billing/meter/get-enterprise-user-usage",
        None,
        session,
        proxy,
        None,
        call::Options::default(),
    )
    .await?;
    // Node: `result.raw ?? {}` → `data?.data || data || result.data || responseData`
    let response_data = result.raw.clone().unwrap_or_else(|| json!({}));
    let usage_data = response_data
        .get("data")
        .and_then(|value| value.get("data"))
        .or_else(|| response_data.get("data"))
        .or_else(|| {
            if result.data.is_null() { None } else { Some(&result.data) }
        })
        .cloned()
        .unwrap_or_else(|| response_data.clone());

    let account_type = session
        .get("account")
        .and_then(|account| account.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let edition_type = if account_type == "ultimate" {
        "ultimate"
    } else if account_type == "exclusive" {
        "exclusive"
    } else {
        "enterprise"
    };

    let limit_num = usage_data.get("limitNum").and_then(Value::as_f64);
    if let Some(limit_num) = limit_num {
        let credit = usage_data
            .get("credit")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        // Node: `usageData.cycleResetTime ? parseTime(usageData.cycleResetTime) : null`
        // 注意：判断的是**原始值**的真值，然后才 parseTime（可能得 0）——
        // 所以有值但解析失败时给的是 0，不是 null
        let refresh_at = match usage_data.get("cycleResetTime") {
            Some(value) if js_truthy(value) => {
                Value::from(parse_time(Some(value)))
            }
            _ => Value::Null,
        };
        if limit_num == -1.0 {
            // limitNum === -1 表示不限量
            return Ok(json!({
                "kind": "enterprise",
                "editionType": edition_type,
                "unlimited": true,
                "usageLeft": commodity::UNLIMITED_USAGE_SENTINEL,
                "usageTotal": commodity::UNLIMITED_USAGE_SENTINEL,
                "usageUsed": js_int_string(credit),
                "refreshAt": refresh_at,
                "resources": [],
                "raw": usage_data,
            }));
        }
        return Ok(json!({
            "kind": "enterprise",
            "editionType": edition_type,
            "unlimited": false,
            "usageLeft": js_int_string(limit_num - credit),
            "usageTotal": js_int_string(limit_num),
            "usageUsed": js_int_string(credit),
            "refreshAt": refresh_at,
            "resources": [],
            "raw": usage_data,
        }));
    }
    Ok(json!({
        "kind": "enterprise",
        "editionType": edition_type,
        "unlimited": false,
        "usageLeft": "0",
        "usageTotal": "0",
        "usageUsed": "0",
        "resources": [],
        "raw": if usage_data.is_null() { Value::Null } else { usage_data },
    }))
}

// ─── 纯函数：`query_usage` 的成果 → 两种摊平形状 ──────────────────
//
// 这两个函数都不发请求、也不看时钟：只是把 `resources[]` 换个形状给不同的消费方。
// 放在 billing 而不是各家适配器里，是因为**资源包的行形状只有这一处定义**
// （`get_personal_usage` 里逐键组装的那个 Map）—— 造形状与换形状分在两个文件里，
// 以后加字段时必有一边被忘掉。

/// 资源包时间字段的 epoch → 毫秒。
///
/// 上游有的字段给秒、有的给毫秒（`CreateTime` 是毫秒，而 `DeductionEndTime`
/// 在参考实现里也按毫秒读）。分界取 1e11：秒级 epoch 到公元 5138 年都不会越过
/// 它，毫秒级从 1973 年起就在它之上 —— 与前端 `subscriptionText` 判 `expireAt`
/// 用的是同一个数。
fn epoch_to_millis(value: i64) -> i64 {
    if value > 100_000_000_000 {
        value
    } else {
        value.saturating_mul(1000)
    }
}

/// `AccountAttributes[]` 里的 `grantReason`（发放原因）；没有 / 空白 → `None`。
fn grant_reason(item: &Value) -> Option<String> {
    let list = item.get("AccountAttributes").and_then(Value::as_array)?;
    for attribute in list {
        if attribute.get("Key").and_then(Value::as_str) != Some("grantReason") {
            continue;
        }
        let value = attribute.get("Value").and_then(Value::as_str).unwrap_or("").trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// 积分简报（`{kind, unlimited, totalLeft, planLeft, bonusLeft}`）。
///
/// 这是账号页余额列的**既有形状**，逐字不变（判别键与消费方见
/// `providers::workbuddy::adapter` 的 `query_usage`）。企业账号没有套餐 / 奖励
/// 之分：`totalLeft` 即剩余额度，后两项为 null（不限量时 totalLeft 也是 null）。
pub fn credits_summary_from(usage: &Value) -> Value {
    let kind = usage.get("kind").and_then(Value::as_str).unwrap_or("personal");
    if kind == "enterprise" {
        let unlimited = usage.get("unlimited").and_then(Value::as_bool).unwrap_or(false);
        return json!({
            "kind": "enterprise",
            "unlimited": unlimited,
            "totalLeft": if unlimited {
                Value::Null
            } else {
                usage.get("usageLeft").and_then(to_int).map(Value::from).unwrap_or(Value::Null)
            },
            "planLeft": Value::Null,
            "bonusLeft": Value::Null,
        });
    }
    let mut total_left: i64 = 0;
    let mut plan_left: i64 = 0;
    let mut bonus_left: i64 = 0;
    if let Some(resources) = usage.get("resources").and_then(Value::as_array) {
        for item in resources {
            let left = item.get("left").and_then(to_int).unwrap_or(0);
            total_left += left;
            let code = item.get("packageCode").and_then(Value::as_str);
            if is_plan_base(code) {
                plan_left += left;
            } else if is_bonus(code) {
                bonus_left += left;
            }
        }
    }
    json!({
        "kind": "personal",
        "unlimited": false,
        "totalLeft": total_left,
        "planLeft": plan_left,
        "bonusLeft": bonus_left,
    })
}

/// `resources[]` → 逐份积分包明细（账号页「积分明细」弹层的输入）。
///
/// 每份一行，字段与上游一一对应：
/// ```text
///   name / daily           这一份是什么
///   total / used / remain  面额 / 已用 / 剩余（上游的数，不做二次计算）
///   grantAt                领取（发放）时刻 —— 上游 CreateTime
///   startAt / expireAt     生效 / 到期（抵扣起止）
///   refreshAt              周期刷新时刻（日额度包没有）
///   grantReason            来源一句话（上游没给则无此键）
/// ```
///
/// ── 为什么直接透传 resources 而不是另算一份 ────────────────────
/// 这些键在 `get_personal_usage` 里已按官方客户端口径解好了（含「缺失 ≠ 0」与
/// 日额度包该读哪个到期字段）。本函数只补两件上游没做的事：改名为界面契约里的
/// 名字、把**未知字段整个删掉**（`timestamp_json` 给缺失写出的是 `null`，而 JS
/// 里 `Number(null) === 0` —— 界面按「有没有这个键」判空，写 null 会被画成 0）。
///
/// 顺序保留上游那份（已按套餐优先级 + 到期时间升序排过，见 `get_personal_usage`），
/// 界面因此不必再排一次。
pub fn credit_packs_from(usage: &Value) -> Vec<Value> {
    let Some(resources) = usage.get("resources").and_then(Value::as_array) else {
        return Vec::new();
    };
    resources.iter().map(credit_pack_of).collect()
}

/// 一份资源包 → 明细行（键名与顺序见 [`credit_packs_from`] 的文档）。
fn credit_pack_of(item: &Value) -> Value {
    let mut row = Map::new();
    let name = item.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
    row.insert("name".to_string(), Value::String(name));
    // 三个数：上游给什么写什么，缺失**不写键**（写成 0 就是那个「读一个不存在的
    // 字段、渲染成剩余 0」的事故）
    for (from, to) in [("total", "total"), ("used", "used"), ("left", "remain")] {
        if let Some(value) = item.get(from) {
            if !value.is_null() {
                row.insert(to.to_string(), value.clone());
            }
        }
    }
    // 四个时刻：同上，null 一律不写
    for to in ["createAt", "startAt", "expireAt", "refreshAt"] {
        if let Some(value) = item.get(to) {
            if !value.is_null() {
                let key = if to == "createAt" { "grantAt" } else { to };
                row.insert(key.to_string(), value.clone());
            }
        }
    }
    // 日额度包（每天重置、无到期作废）：界面对这一档的「到期」另有说法，
    // 所以这一位必须如实带出去，不能靠名字猜
    if item.get("isDaily").and_then(Value::as_bool) == Some(true) {
        row.insert("daily".to_string(), Value::Bool(true));
    }
    if let Some(reason) = item.get("grantReason") {
        if !reason.is_null() {
            row.insert("grantReason".to_string(), reason.clone());
        }
    }
    Value::Object(row)
}
