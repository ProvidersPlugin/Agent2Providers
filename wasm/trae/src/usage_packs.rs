//! 原生权益包读数：三层quota选择、可见包过滤、积分逐包四舍五入、-1不限。
//! 仅投影真实字段，缺limit/usage不写伪造零值；官方明确is_credits_billing且空包才表示零。

use super::util;
use agent2api_wasm_sdk::{json, Value};

#[derive(Clone)]
pub struct Pack {
    pub data: Value,
    pub base: Value,
    pub quota: Value,
}

fn has_quota(value: &Value) -> bool {
    [
        "basic_usage_limit",
        "bonus_usage_limit",
        "premium_model_fast_request_limit",
        "credits_limit",
    ]
    .iter()
    .any(|key| util::number(&value[*key]).is_some())
}

pub fn packs(value: &Value) -> Vec<Pack> {
    value["user_entitlement_pack_list"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|data| {
            let base = &data["entitlement_base_info"];
            if util::integer(&base["product_type"]) == Some(3)
                || base["is_hide"] == true
                || util::integer(&base["status"]) == Some(3)
            {
                return None;
            }
            let quotas = [
                &base["quota"],
                &base["product_extra"]["subscription_extra"]["quota"],
                &base["product_extra"]["package_extra"]["quota"],
            ];
            let quota = quotas
                .iter()
                .find(|quota| has_quota(quota))
                .copied()
                .unwrap_or(quotas[2])
                .clone();
            Some(Pack {
                data: data.clone(),
                base: base.clone(),
                quota,
            })
        })
        .collect()
}

fn positive(value: &Value) -> Option<f64> {
    util::number(value).filter(|n| *n >= 0.0 && *n <= (1_i64 << 53) as f64)
}

fn remaining(pack: &Pack, key: &str, used: &str) -> Option<f64> {
    let limit = positive(&pack.quota[key])?;
    let used = positive(&pack.data["usage"][used])?;
    Some((limit - used).max(0.0))
}

pub fn basic(pack: &Pack) -> Option<f64> {
    let limit = positive(&pack.quota["basic_usage_limit"])?;
    let used = positive(&pack.data["usage"]["basic_usage_amount"])?;
    let mut remaining = (limit - used).max(0.0);
    if let Some(bonus) = positive(&pack.quota["bonus_usage_limit"]) {
        let spent = positive(&pack.data["usage"]["bonus_usage_amount"])?;
        remaining += (bonus - spent).max(0.0);
    }
    Some(remaining.max(0.0))
}

pub fn select(packs: &[Pack]) -> Option<&Pack> {
    [100, 6, 5, 4, 1, 9, 8, 0]
        .iter()
        .find_map(|p| {
            packs
                .iter()
                .find(|pack| util::integer(&pack.base["product_type"]) == Some(*p))
        })
        .or_else(|| packs.first())
}

pub fn plan(pack: &Pack) -> String {
    let text = util::text(&pack.data, &["display_desc"]);
    if !text.is_empty() {
        return util::safe_text(&text, 128);
    }
    match util::integer(&pack.base["product_type"]) {
        Some(100) => "CNExpress",
        Some(6) => "Ultra",
        Some(4 | 5) => "Pro+",
        Some(1 | 9) => "Pro",
        Some(8) => "Lite",
        Some(0) => "Free",
        _ => "Unknown",
    }
    .into()
}

pub struct Balance {
    pub remain: Option<f64>,
    pub total: Option<f64>,
    pub used: Option<f64>,
    pub unlimited: bool,
}

pub fn pool(packs: &[Pack], credits_billing: bool) -> Balance {
    let unlimited = packs
        .iter()
        .any(|p| util::number(&p.quota["credits_limit"]) == Some(-1.0));
    let values: Vec<_> = packs
        .iter()
        .filter(|p| p.quota.get("credits_limit").is_some())
        .collect();
    let known = credits_billing || !values.is_empty();
    let complete = values
        .iter()
        .all(|p| remaining(p, "credits_limit", "credits_amount").is_some());
    let sum: f64 = values
        .iter()
        .filter_map(|p| remaining(p, "credits_limit", "credits_amount"))
        .map(|n| n.round())
        .sum();
    let limits: Option<Vec<_>> = values
        .iter()
        .map(|p| positive(&p.quota["credits_limit"]))
        .collect();
    let used: Option<Vec<_>> = values
        .iter()
        .map(|p| positive(&p.data["usage"]["credits_amount"]))
        .collect();
    Balance {
        remain: (known && complete && !unlimited).then_some(sum),
        total: if known && !unlimited {
            limits.map(|values| values.iter().sum())
        } else {
            None
        },
        used: if known && !unlimited {
            used.map(|values| values.iter().sum())
        } else {
            None
        },
        unlimited,
    }
}

pub fn fast(packs: &[Pack]) -> Option<Balance> {
    let values: Vec<_> = packs
        .iter()
        .filter(|p| {
            p.quota.get("premium_model_fast_request_limit").is_some()
                || p.data["usage"].get("premium_model_fast_amount").is_some()
        })
        .collect();
    if values.is_empty() {
        return None;
    }
    let unlimited = values
        .iter()
        .any(|p| util::number(&p.quota["premium_model_fast_request_limit"]) == Some(-1.0));
    let totals: Option<Vec<_>> = values
        .iter()
        .map(|p| positive(&p.quota["premium_model_fast_request_limit"]))
        .collect();
    let used: Option<Vec<_>> = values
        .iter()
        .map(|p| positive(&p.data["usage"]["premium_model_fast_amount"]))
        .collect();
    let total: Option<f64> = totals.map(|v| v.iter().sum());
    let used: Option<f64> = used.map(|v| v.iter().sum());
    Some(Balance {
        remain: total.zip(used).map(|(a, b): (f64, f64)| (a - b).max(0.0)),
        total,
        used,
        unlimited,
    })
}

pub fn basic_balance(pack: &Pack) -> Balance {
    let unlimited = util::number(&pack.quota["basic_usage_limit"]) == Some(-1.0)
        || util::number(&pack.quota["bonus_usage_limit"]) == Some(-1.0);
    let bonus = if pack.quota.get("bonus_usage_limit").is_some() {
        positive(&pack.quota["bonus_usage_limit"])
    } else {
        Some(0.0)
    };
    let bonus_used = if pack.quota.get("bonus_usage_limit").is_some() {
        positive(&pack.data["usage"]["bonus_usage_amount"])
    } else {
        Some(0.0)
    };
    Balance {
        remain: (!unlimited).then(|| basic(pack)).flatten(),
        total: positive(&pack.quota["basic_usage_limit"])
            .zip(bonus)
            .map(|(basic, bonus)| basic + bonus),
        used: positive(&pack.data["usage"]["basic_usage_amount"])
            .zip(bonus_used)
            .map(|(basic, bonus)| basic + bonus),
        unlimited,
    }
}

fn amount(value: f64) -> Value {
    json!((value * 100.0).round() / 100.0)
}

pub fn credit_rows(packs: &[Pack], unit: &str) -> Vec<Value> {
    let mut rows = Vec::new();
    for pack in packs {
        let mut row = json!({ "name": plan(pack) });
        let credit = util::number(&pack.quota["credits_limit"]);
        let (pool_unit, total, used, remain, unlimited) = if credit.is_some() {
            (
                "积分",
                positive(&pack.quota["credits_limit"]),
                positive(&pack.data["usage"]["credits_amount"]),
                remaining(pack, "credits_limit", "credits_amount"),
                credit == Some(-1.0),
            )
        } else {
            let balance = basic_balance(pack);
            (
                "额度",
                balance.total,
                balance.used,
                balance.remain,
                balance.unlimited,
            )
        };
        if unlimited {
            row["unlimited"] = json!(true);
        } else {
            for (key, value) in [("total", total), ("used", used), ("remain", remain)] {
                if let Some(value) = value {
                    row[key] = amount(value);
                }
            }
        }
        if pool_unit != unit {
            row["unit"] = json!(pool_unit);
        }
        if let Some(at) = util::millis(&pack.base["start_time"]) {
            row["grantAt"] = json!(at);
        }
        if let Some(at) =
            util::millis(&pack.data["expire_time"]).or_else(|| util::millis(&pack.base["end_time"]))
        {
            row["expireAt"] = json!(at);
        }
        rows.push(row);
    }
    rows.sort_by_key(|row| row["expireAt"].as_i64().unwrap_or(i64::MAX));
    rows
}

pub fn pay_number(pay: &Value, quota: bool, keys: &[&str]) -> Option<f64> {
    let layers = if quota {
        vec![&pay["quota"], &pay["entitlementInfo"]["quota"]]
    } else {
        vec![
            &pay["detail"],
            &pay["entitlementInfo"]["detail"],
            &pay["originPayStatusData"]["detail"],
        ]
    };
    layers
        .iter()
        .find_map(|layer| keys.iter().find_map(|key| util::number(&layer[*key])))
}

pub fn solo(packs: &[Pack], pay: &Value) -> (Option<f64>, bool) {
    let flags = [
        "enable_solo_agent",
        "enable_solo_builder",
        "enable_solo_coder",
        "enable_solo_lite",
        "enable_solo_web",
    ];
    let mut layers = vec![&pay["quota"], &pay["entitlementInfo"]["quota"]];
    for p in packs {
        layers.extend([
            &p.base["quota"],
            &p.base["product_extra"]["subscription_extra"]["quota"],
            &p.base["product_extra"]["package_extra"]["quota"],
        ]);
    }
    let parallel = pay_number(pay, true, &["solo_agent_parallel_limit"]).or_else(|| {
        layers
            .iter()
            .find_map(|v| util::number(&v["solo_agent_parallel_limit"]))
    });
    (
        parallel,
        layers
            .iter()
            .any(|layer| flags.iter().any(|key| layer[*key] == true)),
    )
}
