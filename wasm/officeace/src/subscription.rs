//! bonus_skus 是已到账账本，不是待领任务。数字/数字字符串都收，缺读数不伪装成零。
//! 可用积分只聚合未过期项并向下取整；明细列出奖励历史（含过期）和有效套餐。

use super::util;
use agent2api_wasm_sdk::{self as sdk, json, Value};

pub struct Bonus {
    pub id: String,
    pub name: String,
    pub total: Option<f64>,
    pub used: Option<f64>,
    pub expires: i64,
}
impl Bonus {
    pub fn remaining(&self) -> Option<f64> {
        self.total
            .zip(self.used)
            .map(|(total, used)| (total - used).max(0.0))
            .filter(|value| value.is_finite())
    }
}

pub fn bonuses(value: &Value) -> Vec<Bonus> {
    value["bonus_skus"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let id = util::text(row, &["activity_id", "cbc_resource_id", "activity_name"]);
            if id.is_empty() {
                return None;
            }
            let name = util::text(row, &["activity_name"]);
            Some(Bonus {
                name: if name.is_empty() { id.clone() } else { name },
                id,
                total: util::number(&row["points"]).filter(|value| *value >= 0.0),
                used: util::number(&row["current_value"]).filter(|value| *value >= 0.0),
                expires: util::parse_ms(&row["expired_time"]),
            })
        })
        .collect()
}

pub fn checked_in_today(snapshot: &Value) -> bool {
    let today = util::day(sdk::now());
    snapshot["bonus_skus"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|bonus| {
            let resource = util::text(bonus, &["cbc_resource_id"]).to_ascii_lowercase();
            let name = util::text(bonus, &["activity_name"]);
            let effective = util::parse_ms(&bonus["effective_time"]);
            let expiry = util::parse_ms(&bonus["expired_time"]);
            (resource.contains("daily") || name.contains("签到") || name.contains("每日"))
                && effective > 0
                && util::day(effective) == today
                && (expiry <= 0 || expiry > sdk::now())
        })
}

fn whole(value: f64) -> f64 {
    if !value.is_finite() || value <= 0.0 {
        0.0
    } else {
        value.floor()
    }
}
fn remaining(total: Option<f64>, used: Option<f64>) -> Option<f64> {
    total
        .zip(used)
        .map(|(total, used)| total - used)
        .filter(|value| value.is_finite())
        .map(whole)
}
fn add(target: &mut Option<f64>, value: Option<f64>) {
    *target = target
        .zip(value)
        .map(|(left, right)| left + right)
        .filter(|value| value.is_finite());
}

fn row(
    name: &str,
    total: Option<f64>,
    used: Option<f64>,
    remain: Option<f64>,
    expires: i64,
    unit: Option<&str>,
) -> Value {
    let mut value = json!({ "name": name });
    for (key, number) in [("total", total), ("used", used), ("remain", remain)] {
        if let Some(number) = number {
            value[key] = json!(number);
        }
    }
    if expires > 0 {
        value["expireAt"] = json!(expires);
    }
    if let Some(unit) = unit {
        value["unit"] = json!(unit);
    }
    value
}

pub fn summarize(snapshot: &Value) -> Value {
    let now = sdk::now();
    let mut plan_total = snapshot["skus"].is_array().then_some(0.0);
    let mut plan_used = plan_total;
    let mut search_total = plan_total;
    let mut search_used = plan_total;
    let mut name = String::new();
    let mut expiry = 0;
    let mut rows = Vec::new();
    for sku in snapshot["skus"].as_array().into_iter().flatten() {
        let sku_name = util::text(sku, &["sku_name"]);
        let sku_expires = util::parse_ms(&sku["expired_time"]);
        if sku_expires > 0 && sku_expires <= now {
            continue;
        }
        if name.is_empty() {
            name = sku_name.clone();
        }
        if sku_expires > 0 && (expiry == 0 || sku_expires < expiry) {
            expiry = sku_expires;
        }
        let Some(quotas) = sku["quotas"].as_array() else {
            plan_total = None;
            plan_used = None;
            search_total = None;
            search_used = None;
            continue;
        };
        for quota in quotas {
            let quota_expires = util::parse_ms(&quota["expired_time"]);
            let expires = if quota_expires <= 0 {
                sku_expires
            } else if sku_expires <= 0 {
                quota_expires
            } else {
                quota_expires.min(sku_expires)
            };
            if expires > 0 && expires <= now {
                continue;
            }
            let total = util::number(&quota["sku_value"]).filter(|value| *value >= 0.0);
            let used = util::number(&quota["current_value"]).filter(|value| *value >= 0.0);
            match quota["sku_attr_code"].as_str() {
                Some("officeace_points") => {
                    add(&mut plan_total, total);
                    add(&mut plan_used, used);
                    rows.push(row(
                        if sku_name.is_empty() {
                            "套餐积分"
                        } else {
                            &sku_name
                        },
                        total,
                        used,
                        remaining(total, used),
                        expires,
                        None,
                    ));
                }
                Some("online_search_count") => {
                    add(&mut search_total, total);
                    add(&mut search_used, used);
                }
                _ => {}
            }
        }
    }
    let bonuses = bonuses(snapshot);
    let valid_bonuses = snapshot["bonus_skus"]
        .as_array()
        .is_some_and(|rows| rows.len() == bonuses.len());
    let mut bonus_total = valid_bonuses.then_some(0.0);
    let mut bonus_used = bonus_total;
    for bonus in bonuses {
        if bonus.expires <= 0 || bonus.expires > now {
            add(&mut bonus_total, bonus.total);
            add(&mut bonus_used, bonus.used);
        }
        rows.push(row(
            &bonus.name,
            bonus.total,
            bonus.used,
            bonus.remaining().map(whole),
            bonus.expires,
            None,
        ));
    }
    if search_total.is_some_and(|value| value > 0.0) {
        rows.push(row(
            "联网搜索",
            search_total,
            search_used,
            remaining(search_total, search_used),
            0,
            Some("次"),
        ));
    }
    rows.sort_by(|left, right| {
        left["expireAt"]
            .as_i64()
            .unwrap_or(i64::MAX)
            .cmp(&right["expireAt"].as_i64().unwrap_or(i64::MAX))
            .then_with(|| left["name"].as_str().cmp(&right["name"].as_str()))
    });
    let plan = remaining(plan_total, plan_used);
    let bonus = remaining(bonus_total, bonus_used);
    let available = plan.zip(bonus).map(|(left, right)| left + right);
    if name.is_empty() {
        name = "OfficeAce 果办".into();
    }
    let mut value = json!({ "available": available, "unit": "积分", "wallets": [
        { "type": "plan", "displayName": name, "balance": plan },
        { "type": "bonus", "displayName": "奖励积分", "balance": bonus }
    ], "subscription": { "planName": name, "status": snapshot["subscribe_status"], "expireAt": expiry,
        "remainQuota": plan, "totalQuota": plan_total, "searches": if search_total.is_some_and(|value| value > 0.0) {
            json!({ "remaining": remaining(search_total, search_used), "total": search_total })
        } else { Value::Null } }, "raw": snapshot });
    if !rows.is_empty() {
        value["credits"] = json!(rows);
    }
    value["quotaSources"] = sources(&value);
    value
}

pub fn sources(report: &Value) -> Value {
    let mut rows = Vec::new();
    for wallet in report["wallets"].as_array().into_iter().flatten() {
        if let Some(balance) = wallet["balance"].as_f64() {
            rows.push(
                json!({ "id": format!("wallet:{}", wallet["type"].as_str().unwrap_or("")),
                "label": wallet["displayName"], "provider": "officeace", "unit": "积分",
                "remaining": balance, "total": null, "used": null, "unlimited": false }),
            );
        }
    }
    json!(rows)
}
