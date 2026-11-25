//! 原生权益包读数：三层quota选择、可见包过滤、积分两位小数精度、非额度占位包不进明细。
//! 缺usage或缺失用量按0处理，非法负值仍保持未知，-1不限。

use super::util;
use agent2api_wasm_sdk::{json, Value};

#[derive(Clone)]
pub struct Pack {
    pub data: Value,
    pub base: Value,
    pub quota: Value,
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn expire_millis(pack: &Pack) -> Option<i64> {
    util::millis(&pack.data["expire_time"]).or_else(|| util::millis(&pack.base["end_time"]))
}

fn is_expired(pack: &Pack, now: Option<i64>) -> bool {
    matches!((now, expire_millis(pack)), (Some(now), Some(at)) if at <= now)
}

fn used_amount(pack: &Pack, key: &str) -> Option<f64> {
    match pack.data.get("usage") {
        None | Some(Value::Null) => Some(0.0),
        Some(Value::Object(usage)) => match usage.get(key) {
            None | Some(Value::Null) => Some(0.0),
            Some(value) => positive(value),
        },
        Some(_) => None,
    }
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
    let used = used_amount(pack, used)?;
    Some(round2((limit - used).max(0.0)))
}

pub fn basic(pack: &Pack) -> Option<f64> {
    let limit = positive(&pack.quota["basic_usage_limit"])?;
    let used = used_amount(pack, "basic_usage_amount")?;
    let mut left = limit - used;
    if !matches!(
        pack.quota.get("bonus_usage_limit"),
        None | Some(Value::Null)
    ) {
        let bonus = positive(&pack.quota["bonus_usage_limit"])?;
        let spent = used_amount(pack, "bonus_usage_amount")?;
        left += (bonus - spent).max(0.0);
    }
    Some(round2(left.max(0.0)))
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

pub fn pool(packs: &[Pack], credits_billing: bool, now: Option<i64>) -> Balance {
    let values: Vec<_> = packs
        .iter()
        .filter(|p| !is_expired(p, now) && util::number(&p.quota["credits_limit"]).is_some())
        .collect();
    let unlimited = values
        .iter()
        .any(|p| util::number(&p.quota["credits_limit"]) == Some(-1.0));
    let known = credits_billing || !values.is_empty();
    let complete = values
        .iter()
        .all(|p| remaining(p, "credits_limit", "credits_amount").is_some());
    let sum = round2(
        values
            .iter()
            .filter_map(|p| remaining(p, "credits_limit", "credits_amount"))
            .sum(),
    );
    let limits: Option<Vec<_>> = values
        .iter()
        .map(|p| positive(&p.quota["credits_limit"]))
        .collect();
    let used: Option<Vec<_>> = values
        .iter()
        .map(|p| used_amount(p, "credits_amount"))
        .collect();
    Balance {
        remain: (known && complete && !unlimited).then_some(sum),
        total: if known && !unlimited {
            limits.map(|values| round2(values.iter().sum()))
        } else {
            None
        },
        used: if known && !unlimited {
            used.map(|values| round2(values.iter().sum()))
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
            util::number(&p.quota["premium_model_fast_request_limit"]).is_some()
                || util::number(&p.data["usage"]["premium_model_fast_amount"]).is_some()
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
        .map(|p| used_amount(p, "premium_model_fast_amount"))
        .collect();
    let total: Option<f64> = totals.map(|v| round2(v.iter().sum()));
    let used: Option<f64> = used.map(|v| round2(v.iter().sum()));
    Some(Balance {
        remain: (!unlimited)
            .then(|| total.zip(used).map(|(a, b): (f64, f64)| round2((a - b).max(0.0))))
            .flatten(),
        total: (!unlimited).then_some(total).flatten(),
        used: (!unlimited).then_some(used).flatten(),
        unlimited,
    })
}

pub fn basic_balance(pack: &Pack) -> Balance {
    if matches!(
        pack.quota.get("basic_usage_limit"),
        None | Some(Value::Null)
    ) && matches!(
        pack.quota.get("bonus_usage_limit"),
        None | Some(Value::Null)
    ) {
        return Balance {
            remain: None,
            total: None,
            used: None,
            unlimited: false,
        };
    }
    let unlimited = util::number(&pack.quota["basic_usage_limit"]) == Some(-1.0)
        || util::number(&pack.quota["bonus_usage_limit"]) == Some(-1.0);
    let has_bonus = !matches!(
        pack.quota.get("bonus_usage_limit"),
        None | Some(Value::Null)
    );
    let bonus = if has_bonus {
        positive(&pack.quota["bonus_usage_limit"])
    } else {
        Some(0.0)
    };
    let bonus_used = if has_bonus {
        used_amount(pack, "bonus_usage_amount")
    } else {
        Some(0.0)
    };
    Balance {
        remain: (!unlimited).then(|| basic(pack)).flatten(),
        total: (!unlimited)
            .then(|| {
                positive(&pack.quota["basic_usage_limit"])
                    .zip(bonus)
                    .map(|(basic, bonus)| round2(basic + bonus))
            })
            .flatten(),
        used: (!unlimited)
            .then(|| {
                used_amount(pack, "basic_usage_amount")
                    .zip(bonus_used)
                    .map(|(basic, bonus)| round2(basic + bonus))
            })
            .flatten(),
        unlimited,
    }
}

fn amount(value: f64) -> Value {
    let rounded = round2(value);
    if rounded.fract() == 0.0 && rounded.abs() <= (1_i64 << 53) as f64 {
        json!(rounded as i64)
    } else {
        json!(rounded)
    }
}

pub fn credit_rows(packs: &[Pack], unit: &str) -> Vec<Value> {
    let mut rows: Vec<(i64, String, Value)> = Vec::new();
    for pack in packs {
        let name = plan(pack);
        let credit = util::number(&pack.quota["credits_limit"]);
        let fast = util::number(&pack.quota["premium_model_fast_request_limit"]);
        let (pool_unit, total, used, remain, unlimited) =
            if credit == Some(-1.0) || credit.is_some_and(|v| v > 0.0) {
                (
                    "积分",
                    positive(&pack.quota["credits_limit"]),
                    used_amount(pack, "credits_amount"),
                    remaining(pack, "credits_limit", "credits_amount"),
                    credit == Some(-1.0),
                )
            } else if fast == Some(-1.0) || fast.is_some_and(|v| v > 0.0) {
                (
                    "次",
                    positive(&pack.quota["premium_model_fast_request_limit"]),
                    used_amount(pack, "premium_model_fast_amount"),
                    remaining(
                        pack,
                        "premium_model_fast_request_limit",
                        "premium_model_fast_amount",
                    ),
                    fast == Some(-1.0),
                )
            } else if util::number(&pack.quota["basic_usage_limit"]).is_some() {
                let balance = basic_balance(pack);
                (
                    "额度",
                    balance.total,
                    balance.used,
                    balance.remain,
                    balance.unlimited,
                )
            } else {
                continue;
            };
        let has_capacity = unlimited
            || total.is_some_and(|v| v > 0.0)
            || remain.is_some_and(|v| v > 0.0)
            || used.is_some_and(|v| v > 0.0);
        if !has_capacity {
            continue;
        }
        let mut row = json!({ "name": name });
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
        let expire = expire_millis(pack);
        if let Some(at) = expire {
            row["expireAt"] = json!(at);
        }
        rows.push((expire.unwrap_or(i64::MAX), name, row));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    rows.into_iter().map(|(_, _, row)| row).collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_non_quota_free_shell_and_defaults_omitted_usage_to_zero() {
        let now = 1_795_700_000_000_i64;
        let ent = json!({
            "is_credits_billing": true,
            "user_entitlement_pack_list": [
                {
                    "display_desc": "免费",
                    "expire_time": 1_796_054_399_i64,
                    "entitlement_base_info": {
                        "product_type": 0,
                        "start_time": 1_793_462_400_i64,
                        "end_time": 1_796_054_399_i64,
                        "quota": {
                            "enable_solo_builder": true,
                            "solo_agent_parallel_limit": 2
                        }
                    }
                },
                {
                    "display_desc": "每月登录赠送",
                    "expire_time": 1_796_054_399_i64,
                    "entitlement_base_info": {
                        "product_type": 0,
                        "start_time": 1_793_462_400_i64,
                        "end_time": 1_796_054_399_i64,
                        "quota": { "credits_limit": 500 }
                    },
                    "usage": { "credits_amount": 0.08 }
                },
                {
                    "display_desc": "新用户福利",
                    "expire_time": 1_797_927_600_i64,
                    "entitlement_base_info": {
                        "product_type": 11,
                        "start_time": 1_795_249_200_i64,
                        "end_time": 1_797_927_600_i64,
                        "quota": { "credits_limit": 1500 }
                    }
                },
                {
                    "display_desc": "签到奖励",
                    "expire_time": 1_798_094_040_i64,
                    "entitlement_base_info": {
                        "product_type": 11,
                        "start_time": 1_795_415_640_i64,
                        "end_time": 1_798_094_040_i64,
                        "quota": { "credits_limit": 300 }
                    },
                    "usage": {}
                }
            ]
        });

        let parsed = packs(&ent);
        assert_eq!(parsed.len(), 4);
        assert_eq!(select(&parsed).map(plan).as_deref(), Some("免费"));
        assert_eq!(solo(&parsed, &Value::Null), (Some(2.0), true));

        let summary = pool(&parsed, true, Some(now));
        assert_eq!(summary.remain, Some(2299.92));
        assert_eq!(summary.total, Some(2300.0));
        assert_eq!(summary.used, Some(0.08));

        let rows = credit_rows(&parsed, "积分");
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| row["name"] != "免费"));
        assert!(rows.iter().all(|row| row.get("unit").is_none()));
        assert_eq!(rows[0]["name"], "每月登录赠送");
        assert_eq!(rows[0]["remain"], json!(499.92));
        assert_eq!(rows[0]["total"], json!(500));
        assert_eq!(rows[0]["used"], json!(0.08));
        assert_eq!(rows[1]["name"], "新用户福利");
        assert_eq!(rows[1]["remain"], json!(1500));
        assert_eq!(rows[1]["total"], json!(1500));
        assert_eq!(rows[1]["used"], json!(0));
        assert_eq!(rows[2]["name"], "签到奖励");
        assert_eq!(rows[2]["remain"], json!(300));
        assert_eq!(rows[2]["total"], json!(300));
        assert_eq!(rows[2]["used"], json!(0));
    }

    #[test]
    fn skips_zero_placeholder_keeps_expired_or_exhausted_rows_and_preserves_invalid_usage() {
        let now = 1_795_700_000_000_i64;
        let ent = json!({
            "is_credits_billing": true,
            "user_entitlement_pack_list": [
                {
                    "display_desc": "免费",
                    "expire_time": 1_796_054_399_i64,
                    "entitlement_base_info": {
                        "product_type": 0,
                        "quota": { "basic_usage_limit": 0 }
                    }
                },
                {
                    "display_desc": "已过期积分",
                    "expire_time": 1_795_600_000_i64,
                    "entitlement_base_info": {
                        "product_type": 8,
                        "quota": { "credits_limit": 100 }
                    }
                },
                {
                    "display_desc": "已用完积分",
                    "expire_time": 1_796_054_399_i64,
                    "entitlement_base_info": {
                        "product_type": 8,
                        "quota": { "credits_limit": 100 }
                    },
                    "usage": { "credits_amount": 100 }
                }
            ]
        });

        let parsed = packs(&ent);
        let summary = pool(&parsed, true, Some(now));
        assert_eq!(summary.remain, Some(0.0));
        assert_eq!(summary.total, Some(100.0));

        let rows = credit_rows(&parsed, "积分");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["name"], "已过期积分");
        assert_eq!(rows[0]["remain"], json!(100));
        assert_eq!(rows[1]["name"], "已用完积分");
        assert_eq!(rows[1]["remain"], json!(0));
        assert_eq!(rows[1]["total"], json!(100));
        assert_eq!(rows[1]["used"], json!(100));

        let bad_ent = json!({
            "user_entitlement_pack_list": [{
                "display_desc": "异常包",
                "entitlement_base_info": { "product_type": 8, "quota": { "credits_limit": 100 } },
                "usage": { "credits_amount": -5 }
            }]
        });
        let bad_packs = packs(&bad_ent);
        assert_eq!(pool(&bad_packs, false, Some(now)).remain, None);
        let bad_rows = credit_rows(&bad_packs, "积分");
        assert_eq!(bad_rows.len(), 1);
        assert!(bad_rows[0].get("remain").is_none());
        assert!(bad_rows[0].get("used").is_none());
    }

    #[test]
    fn preserves_basic_and_fast_packs_in_credit_rows() {
        let ent = json!({
            "user_entitlement_pack_list": [
                {
                    "display_desc": "Pro",
                    "expire_time": 1_796_054_399_i64,
                    "entitlement_base_info": {
                        "product_type": 1,
                        "quota": { "basic_usage_limit": 500 }
                    }
                },
                {
                    "display_desc": "速通包",
                    "expire_time": 1_796_100_000_i64,
                    "entitlement_base_info": {
                        "product_type": 9,
                        "quota": { "premium_model_fast_request_limit": 600 }
                    },
                    "usage": { "premium_model_fast_amount": 100 }
                }
            ]
        });

        let parsed = packs(&ent);
        let rows = credit_rows(&parsed, "积分");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["name"], "Pro");
        assert_eq!(rows[0]["unit"], "额度");
        assert_eq!(rows[0]["remain"], json!(500));
        assert_eq!(rows[0]["used"], json!(0));
        assert_eq!(rows[1]["name"], "速通包");
        assert_eq!(rows[1]["unit"], "次");
        assert_eq!(rows[1]["remain"], json!(500));
        assert_eq!(rows[1]["used"], json!(100));
    }
}
