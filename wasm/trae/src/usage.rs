//! 积分池、速通、套餐与逐包到期明细；两接口HTTP200也过业务错误门。
//! 不回传raw账号响应或设备号。缺失用量保持未知，-1用unlimited表达。

use super::{
    client, credentials, endpoints, headers, refresh,
    usage_packs::{self as packs, Balance},
    util,
};
use agent2api_wasm_sdk::{json, Error, Result, Value};

fn wallet(kind: &str, name: &str, unit: &str, balance: &Balance) -> Value {
    let mut value = json!({ "type": kind, "displayName": name, "unit": unit, "unlimited": balance.unlimited,
        "balanceView": if balance.unlimited { "不限".to_string() } else if let Some(n) = balance.remain { format!("剩 {n} {unit}") } else { "未知".into() } });
    if !balance.unlimited {
        for (key, value_num) in [
            ("balance", balance.remain),
            ("total", balance.total),
            ("used", balance.used),
        ] {
            if let Some(number) = value_num {
                value[key] = json!(number);
            }
        }
    }
    value
}

pub fn sources(report: &Value) -> Value {
    let sources: Vec<Value> = report["wallets"].as_array().into_iter().flatten().filter_map(|wallet| {
        let id = wallet["type"].as_str()?;
        if !matches!(id, "credits_pool" | "fast_request" | "plan_quota") { return None; }
        let mut out = json!({ "id": id, "provider": crate::PROVIDER_ID, "label": wallet["displayName"],
            "unit": wallet["unit"], "unlimited": wallet["unlimited"] == true });
        for (target, field) in [("remaining", "balance"), ("total", "total"), ("used", "used")] {
            if let Some(value) = util::number(&wallet[field]) { out[target] = json!(value); }
        }
        Some(out)
    }).collect();
    json!(sources)
}

fn read(secret: &Value) -> Result<Value> {
    let device = util::text(secret, &["deviceId"]);
    let ent = client::post(
        &format!("{}{}", endpoints::ACCOUNT, endpoints::ENT_USAGE),
        headers::ug(secret, &device, false)?,
        &json!({}),
    )?
    .accepted()?;
    if ent["user_entitlement_pack_list"]
        .as_array()
        .is_none_or(|rows| {
            rows.len() > 4096
                || rows
                    .iter()
                    .any(|row| !row["entitlement_base_info"].is_object())
        })
    {
        return Err(Error::http(
            502,
            "Trae 权益包响应缺少有效清单，未把缺失读数记成零",
        ));
    }
    let (pay, warning) = match client::post(
        &format!("{}{}", endpoints::ACCOUNT, endpoints::PAY_STATUS),
        headers::ug(secret, &device, false)?,
        &json!({}),
    )
    .and_then(|reply| reply.accepted())
    {
        Ok(pay) => (pay, None),
        Err(error) if error.code.as_deref() == Some("trae_session_expired") => return Err(error),
        Err(error) => (Value::Null, Some(error.message)),
    };
    let packs = packs::packs(&ent);
    let pool = packs::pool(&packs, ent["is_credits_billing"] == true);
    let fast = packs::fast(&packs);
    let selected = packs::select(&packs);
    let basic = selected.map(packs::basic_balance);
    let (available, unit) = if pool.unlimited {
        (None, "积分")
    } else if pool.remain.is_some() {
        (pool.remain, "积分")
    } else if let Some(fast) = &fast {
        (fast.remain.filter(|_| !fast.unlimited), "次")
    } else if let Some(basic) = &basic {
        (basic.remain.filter(|_| !basic.unlimited), "额度")
    } else {
        (None, "积分")
    };
    let mut wallets = Vec::new();
    if pool.remain.is_some() || pool.unlimited {
        wallets.push(wallet("credits_pool", "积分池", "积分", &pool));
    }
    if let Some(fast) = &fast {
        wallets.push(wallet("fast_request", "速通次数", "次", fast));
    } else if let Some(basic) = &basic {
        if basic.total.is_some() || basic.unlimited {
            wallets.push(wallet("plan_quota", "套餐余量", "额度", basic));
        }
    }
    let (parallel, solo) = packs::solo(&packs, &pay);
    if let Some(number) = parallel {
        wallets.push(json!({ "type": "solo_parallel", "displayName": "SOLO并发", "balance": number, "balanceView": format!("{number} 路") }));
    }
    if solo {
        wallets.push(
            json!({ "type": "solo_package", "displayName": "SOLO套餐", "balanceView": "可用" }),
        );
    }
    if let Some(number) = packs::pay_number(&pay, false, &["fast_request_per", "fastRequestPer"]) {
        wallets.push(json!({ "type": "fast_request_per", "displayName": "快请求配额", "balance": number, "balanceView": format!("{number} 次/月") }));
    }
    if let Some((total, consumed)) = util::number(&ent["usage_summary"]["total_amount"])
        .zip(util::number(&ent["usage_summary"]["consumed_amount"]))
    {
        wallets.push(json!({ "type": "usage_summary", "displayName": "官方汇总", "balance": total-consumed,
            "balanceView": format!("剩 {:.2} / 共 {:.2}（已用 {:.2}）", total-consumed,total,consumed) }));
    }
    let plan = selected
        .map(packs::plan)
        .unwrap_or_else(|| "Unknown".into());
    let mut subscription = json!({ "planName": plan });
    let plan_type = util::text(&pay, &["user_pay_identity_str"]);
    if plan != "Unknown" || !plan_type.is_empty() {
        let free = [plan.as_str(), plan_type.as_str()]
            .iter()
            .any(|v| v.to_ascii_lowercase().contains("free") || v.contains("免费"));
        subscription["status"] = json!(if free { "免费" } else { "付费" });
    }
    if let Some(selected) = selected {
        if let Some(at) = util::millis(&selected.base["end_time"]) {
            subscription["expireAt"] = json!(at);
        }
    }
    if fast.is_none() {
        if let Some(basic) = &basic {
            if !basic.unlimited {
                if let Some(value) = basic.remain {
                    subscription["remainQuota"] = json!(value);
                }
                if let Some(value) = basic.total {
                    subscription["totalQuota"] = json!(value);
                }
            }
        }
    }
    let mut report = json!({ "provider": crate::PROVIDER_ID, "available": available, "unit": unit,
        "plan": plan, "wallets": wallets, "subscription": subscription });
    let rows = packs::credit_rows(&packs, unit);
    if !rows.is_empty() {
        report["credits"] = json!(rows);
    }
    if let Some(message) = warning {
        report["error"] = json!(message);
    }
    report["quotaSources"] = sources(&report);
    Ok(report)
}

pub fn query() -> Result<Value> {
    let mut record = refresh::ensure(false, None)?;
    for attempt in 0..2 {
        let secret = credentials::parse(&record["secret"])?;
        match read(&secret) {
            Err(error)
                if attempt == 0
                    && error.code.as_deref() == Some("trae_session_expired")
                    && !util::text(&secret, &["refreshToken"]).is_empty() =>
            {
                record = refresh::ensure(true, Some(&record["secret"]))?;
            }
            Ok(report) => {
                super::account::unchanged(&record["secret"])?;
                return Ok(report);
            }
            Err(error) => return Err(error),
        }
    }
    Err(super::errors::expired())
}
