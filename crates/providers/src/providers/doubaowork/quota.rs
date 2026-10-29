use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::{api, PROVIDER_ID};
use crate::errors::GatewayError;

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|number| number.is_finite() && *number >= 0.0)
}

fn stamp(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|time| *time > 0)
}

fn limits(section: &Value) -> impl Iterator<Item = (&Value, &Value)> {
    section["window_limit_groups"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|group| {
            group["window_limits"]
                .as_array()
                .into_iter()
                .flatten()
                .map(move |limit| (group, limit))
        })
}

fn usable(section: &Value) -> bool {
    section["usage_exhausted"] != true
        && limits(section)
            .any(|(_, limit)| matches!(api::number(&limit["window_type"]), Some(1 | 2 | 4)))
}

fn plan(value: &Value) -> Option<String> {
    let data = value.get("data").unwrap_or(value);
    [
        data.get("membership_display_name"),
        data.pointer("/current_subscription/display/short_name"),
        data.pointer("/current_subscription/display/product_name"),
        data.pointer("/subscription/display/short_name"),
    ]
    .into_iter()
    .flatten()
    .find_map(|value| {
        value
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    })
}

fn remaining_percent(limit: &Value) -> Option<f64> {
    number(&limit["used_percent"])
        .map(|used| (100.0 - used).clamp(0.0, 100.0))
        .or_else(|| {
            let total = number(&limit["total_amount"])?;
            let used = number(&limit["used_amount"])?;
            (total > 0.0).then_some(((total - used) / total * 100.0).clamp(0.0, 100.0))
        })
}

fn bucket(limit: &Value) -> (Option<f64>, Option<f64>, Option<f64>) {
    let total = number(&limit["total_amount"]);
    let used = number(&limit["used_amount"]);
    (
        total.zip(used).map(|(total, used)| (total - used).max(0.0)),
        total,
        used,
    )
}

pub async fn query(ctx: &PluginCtx, account: &str) -> Result<Value, GatewayError> {
    let (_, credentials, client) = api::account(ctx, account, false).await?;
    let overview = api::post(
        &client,
        &credentials,
        "/alice/commerce/sale/subscription/overview/",
        &json!({"product_line":"membership"}),
    )
    .await?;
    let entry = api::post(
        &client,
        &credentials,
        "/alice/commerce/sale/subscription/entry/config/",
        &json!({}),
    )
    .await;
    let data = overview.get("data").unwrap_or(&overview);
    let personal = &data["window_limit_section"];
    let enterprise = &data["enterprise_window_limit_section"];
    let has_personal = data["member_info"]["hasActiveSubscription"] == true;
    let has_enterprise = data["member_info"]["hasEnterpriseSubscription"] == true;
    let enterprise_selected =
        has_enterprise && (usable(enterprise) || !has_personal || !usable(personal));
    let section = if enterprise_selected {
        enterprise
    } else if has_personal {
        personal
    } else {
        &Value::Null
    };
    let mut windows = Vec::new();
    let mut credits = Vec::new();
    let mut sources = Vec::new();
    let window_limits = limits(section)
        .filter(|(_, limit)| matches!(api::number(&limit["window_type"]), Some(1 | 2 | 4)))
        .chain(limits(&data["quota_package_section"]).filter(|(_, limit)| {
            !enterprise_selected && api::number(&limit["window_type"]) == Some(3)
        }));
    for (index, (group, limit)) in window_limits.enumerate() {
        let (code, label) = match api::number(&limit["window_type"]) {
            Some(1) => ("5h", "5 小时"),
            Some(2) => ("weekly", "近 7 天"),
            Some(4) => ("monthly", "每月"),
            _ => ("subscription", "订阅额度"),
        };
        let group_name = group["feature_group_name"].as_str().unwrap_or("");
        let key = format!("{code}-{index}");
        let unlimited =
            limit["unlimited"] == true || (code == "5h" && limit["exemption"]["active"] == true);
        let percent = remaining_percent(limit);
        let reset = if code == "5h" && limit["exemption"]["active"] == true {
            stamp(&limit["exemption"]["end_time"])
        } else {
            stamp(&limit["end_time"])
        };
        let (remain, total, used) = bucket(limit);
        let view = if unlimited {
            "不限".to_string()
        } else if let Some(remain) = remain {
            total
                .map(|total| format!("剩 {remain:.2} / {total:.2}"))
                .unwrap_or_else(|| format!("剩 {remain:.2}"))
        } else {
            percent
                .map(|percent| format!("剩余 {percent:.0}%"))
                .unwrap_or_else(|| "—".into())
        };
        windows.push(json!({"key":key,"label":format!("{group_name} {label}").trim(),"group":group_name,"window":code,
            "remainingPercent":if unlimited {None} else {percent},"view":view,"resetsAt":reset,"unlimited":unlimited}));
        let mut source = crate::quota::source(
            PROVIDER_ID,
            &key,
            &format!("{group_name} {label}"),
            "额度",
            remain,
            total,
            used,
        );
        source["remainingPercent"] = json!(percent);
        source["resetsAt"] = json!(reset);
        source["unlimited"] = json!(unlimited);
        sources.push(source);
        credits.push(json!({"id":key,"name":format!("{group_name} {label}").trim(),"remain":remain,"total":total,"used":used,"unit":"额度","remainingPercent":percent,"unlimited":unlimited,"resetAt":reset,"rolling":true}));
    }
    let bonus = if enterprise_selected {
        None
    } else {
        limits(&data["incentive_quota_section"])
            .find(|(_, limit)| api::number(&limit["window_type"]) == Some(3))
            .map(|(_, limit)| limit)
    };
    let mut bonus_remaining = None;
    if let Some(bonus) = bonus {
        let (remain, total, used) = bucket(bonus);
        bonus_remaining = remain;
        let expiry = stamp(&bonus["end_time"]);
        let percent = remaining_percent(bonus);
        let view = remain
            .map(|remain| format!("剩 {remain:.2} 额度"))
            .unwrap_or_else(|| {
                percent
                    .map(|percent| format!("剩余 {percent:.0}%"))
                    .unwrap_or_else(|| "—".into())
            });
        windows.push(json!({"key":"bonus","label":"奖励额度","group":"奖励额度","view":view,"remainingPercent":percent}));
        credits.push(json!({"id":"bonus","name":"奖励额度","remain":remain,"total":total,"used":used,"unit":"额度","expireAt":expiry}));
        let mut source = crate::quota::source(
            PROVIDER_ID,
            "bonus",
            "奖励额度",
            "额度",
            remain,
            total,
            used,
        );
        source["endsAt"] = json!(expiry);
        source["remainingPercent"] = json!(percent);
        sources.push(source);
    }
    let plan_name = entry
        .as_ref()
        .ok()
        .and_then(plan)
        .or_else(|| plan(&overview));
    let view = if !windows.is_empty() {
        "分窗口额度"
    } else {
        "暂无可用额度"
    };
    let mut report = json!({"provider":PROVIDER_ID,"unit":"额度","availableView":view,"windows":windows,"credits":credits,"quotaSources":sources,
        "subscription":{"planName":plan_name},"bonusLeft":bonus_remaining});
    if let Err(error) = entry {
        report["warnings"] = json!([error.message]);
    }
    Ok(report)
}
