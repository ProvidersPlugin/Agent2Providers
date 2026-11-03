//! 积分汇总、资源包与企业额度保持既有前端契约；未知数值不伪装成零。

use chrono::{FixedOffset, NaiveDateTime, TimeZone};
use serde_json::{json, Map, Value};

use crate::errors::GatewayError;
use crate::json::json_number;

use super::commodity::{
    self, codes, is_bonus, is_daily_credit, is_plan_base, label_of, plan_priority,
};
use super::tools::{js_int_string, time_or_null, to_int};
use super::{call, resources};

pub async fn query_usage(
    session: &Value,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    locale: Option<&str>,
) -> Result<Value, GatewayError> {
    if session
        .pointer("/account/enterpriseId")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.trim().is_empty())
    {
        get_enterprise_usage(session, proxy).await
    } else {
        get_personal_usage(session, proxy, locale).await
    }
}

async fn get_personal_usage(
    session: &Value,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    locale: Option<&str>,
) -> Result<Value, GatewayError> {
    let fetched = resources::fetch(session, proxy, locale).await?;
    let mut packs: Vec<Value> = fetched.accounts.iter().map(resource_of).collect();
    packs.sort_by(|a, b| {
        plan_priority(a.get("packageCode").and_then(Value::as_str))
            .cmp(&plan_priority(b.get("packageCode").and_then(Value::as_str)))
            .then_with(|| {
                a.get("expireAt")
                    .and_then(Value::as_i64)
                    .unwrap_or(i64::MAX)
                    .cmp(
                        &b.get("expireAt")
                            .and_then(Value::as_i64)
                            .unwrap_or(i64::MAX),
                    )
            })
    });
    let aggregate = |field: &str| {
        packs.iter().try_fold(0.0, |sum, item| {
            resources::number(item.get(field)).map(|value| sum + value)
        })
    };
    let total = fetched
        .summary
        .map(|(total, _)| total)
        .or_else(|| aggregate("total"));
    let left = fetched
        .summary
        .map(|(_, left)| left)
        .or_else(|| aggregate("left"));
    let used = total.zip(left).map(|(total, left)| (total - left).max(0.0));
    let find = |wanted: &[&str]| {
        packs.iter().find(|item| {
            item.get("packageCode")
                .and_then(Value::as_str)
                .is_some_and(|code| wanted.contains(&code))
        })
    };
    let flagship = find(&[codes::FLAGSHIP]);
    let advanced = find(&[codes::ADVANCED]);
    let pro = find(&[codes::PRO_YEAR, codes::PRO_MON, codes::PRO_MON_PLUS]);
    let youth = find(&[codes::YOUTH]);
    let trial = find(&[codes::PRO_TRIAL_MON, codes::PRO_TRIAL_YEAR]);
    let active = flagship
        .or(advanced)
        .or(pro)
        .or(youth)
        .or(trial)
        .or_else(|| packs.first());
    let edition = if flagship.is_some() {
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
        "kind": "personal", "editionType": edition, "isPro": pro.is_some(),
        "isTrial": trial.is_some() && pro.is_none() && advanced.is_none() && flagship.is_none() && youth.is_none(),
        "planName": active.and_then(|item| item.get("name")).and_then(Value::as_str).unwrap_or(""),
        "packageCode": active.and_then(|item| item.get("packageCode")),
        "usageTotal": total.map(js_int_string), "usageLeft": left.map(js_int_string), "usageUsed": used.map(js_int_string),
        "expireAt": active.and_then(|item| item.get("expireAt")),
        "refreshAt": active.and_then(|item| item.get("refreshAt")),
        "autoRenew": active.and_then(|item| item.get("autoRenew")).and_then(Value::as_bool).unwrap_or(false),
        "resources": packs,
    }))
}

fn resource_of(item: &Value) -> Value {
    let code = item
        .get("PackageCode")
        .and_then(Value::as_str)
        .unwrap_or("");
    let daily = is_daily_credit(code);
    let (total, left) = resources::capacity(item);
    let name = item
        .get("PackageName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .or_else(|| label_of(code))
        .unwrap_or(code);
    let mut pack = Map::new();
    pack.insert("name".into(), json!(name));
    pack.insert("packageCode".into(), json!(code));
    pack.insert("isDaily".into(), json!(daily));
    if let Some(id) = item.get("ResourceId").filter(|id| !id.is_null()) {
        pack.insert("id".into(), id.clone());
    }
    for (key, value) in [
        ("total", total),
        ("left", left),
        (
            "used",
            total.zip(left).map(|(total, left)| (total - left).max(0.0)),
        ),
    ] {
        if let Some(value) = value {
            pack.insert(key.into(), json_number(value));
        }
    }
    let start = billing_time(item.get("DeductionStartTime"))
        .or_else(|| billing_time(item.get("CycleStartTime")));
    let expire = if daily {
        billing_time(item.get("CycleEndTime"))
    } else {
        billing_time(item.get("DeductionEndTime"))
            .or_else(|| billing_time(item.get("ExpiredTime")))
            .or_else(|| billing_time(item.get("PackageEndTime")))
            .or_else(|| billing_time(item.get("CycleEndTime")))
    };
    let refresh = if daily {
        None
    } else {
        billing_time(item.get("CycleEndTime")).map(|time| time.saturating_add(1000))
    };
    for (key, value) in [
        ("startAt", start),
        ("createAt", billing_time(item.get("CreateTime"))),
        ("expireAt", expire),
        ("refreshAt", refresh),
    ] {
        if let Some(value) = value {
            pack.insert(key.into(), json!(value));
        }
    }
    pack.insert(
        "autoRenew".into(),
        json!(resources::number(item.get("AutoRenewFlag")) == Some(1.0)),
    );
    if let Some(reason) = item
        .get("AccountAttributes")
        .and_then(Value::as_array)
        .and_then(|attributes| {
            attributes.iter().find(|attribute| {
                attribute.get("Key").and_then(Value::as_str) == Some("grantReason")
            })
        })
        .and_then(|attribute| attribute.get("Value"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
    {
        pack.insert("grantReason".into(), json!(reason));
    }
    Value::Object(pack)
}

fn billing_time(value: Option<&Value>) -> Option<i64> {
    if let Some(text) = value.and_then(Value::as_str) {
        if let Ok(naive) = NaiveDateTime::parse_from_str(text.trim(), "%Y-%m-%d %H:%M:%S") {
            return FixedOffset::east_opt(8 * 3600)?
                .from_local_datetime(&naive)
                .single()
                .map(|time| time.timestamp_millis());
        }
    }
    time_or_null(value).filter(|time| *time > 0).map(|time| {
        if time < 100_000_000_000 {
            time.saturating_mul(1000)
        } else {
            time
        }
    })
}

async fn get_enterprise_usage(
    session: &Value,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
) -> Result<Value, GatewayError> {
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
    let usage = result.data.get("data").unwrap_or(&result.data);
    let limit = usage
        .get("limitNum")
        .or_else(|| usage.get("limit_num"))
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        })
        .filter(|value| value.is_finite());
    let credit = resources::number(usage.get("credit").or_else(|| usage.get("used_num")));
    let unlimited = limit == Some(-1.0);
    let total = limit.filter(|value| *value >= 0.0);
    let left = total
        .zip(credit)
        .map(|(limit, credit)| (limit - credit).max(0.0));
    let account_type = session
        .pointer("/account/type")
        .and_then(Value::as_str)
        .unwrap_or("");
    let edition = match account_type {
        "ultimate" => "ultimate",
        "exclusive" => "exclusive",
        _ => "enterprise",
    };
    Ok(json!({
        "kind": "enterprise", "editionType": edition, "unlimited": unlimited,
        "usageLeft": if unlimited { Some(commodity::UNLIMITED_USAGE_SENTINEL.to_string()) } else { left.map(js_int_string) },
        "usageTotal": if unlimited { Some(commodity::UNLIMITED_USAGE_SENTINEL.to_string()) } else { total.map(js_int_string) },
        "usageUsed": credit.map(js_int_string), "refreshAt": billing_time(usage.get("cycleResetTime")),
        "resources": [],
    }))
}

pub fn credits_summary_from(usage: &Value) -> Value {
    let kind = usage
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("personal");
    let unlimited = usage
        .get("unlimited")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if kind == "enterprise" {
        return json!({
            "kind": kind, "unlimited": unlimited,
            "totalLeft": if unlimited { None } else { usage.get("usageLeft").and_then(to_int) },
            "planLeft": Value::Null, "bonusLeft": Value::Null,
        });
    }
    let sum = |filter: fn(Option<&str>) -> bool| -> Option<i64> {
        usage
            .get("resources")
            .and_then(Value::as_array)?
            .iter()
            .filter(|item| filter(item.get("packageCode").and_then(Value::as_str)))
            .try_fold(0.0, |sum, item| {
                resources::number(item.get("left")).map(|left| sum + left)
            })
            .map(|sum| sum.floor() as i64)
    };
    json!({
        "kind": "personal", "unlimited": false,
        "totalLeft": usage.get("usageLeft").and_then(to_int),
        "planLeft": sum(is_plan_base), "bonusLeft": sum(is_bonus),
    })
}

pub(in crate::providers::workbuddy) fn subscription_of(usage: &Value) -> Option<Value> {
    if usage.get("kind").and_then(Value::as_str) == Some("enterprise") {
        return None;
    }
    let edition = usage.get("editionType").and_then(Value::as_str)?;
    let name = match edition {
        "free" => "Free",
        "pro_trial" => "Trial",
        _ => usage
            .get("planName")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())?,
    };
    let mut subscription = json!({
        "planName": name,
        "status": match edition { "free" => "免费", "pro_trial" => "试用", _ => "付费" },
    });
    for (from, to) in [
        ("expireAt", "expireAt"),
        ("usageLeft", "remainQuota"),
        ("usageTotal", "totalQuota"),
    ] {
        if let Some(value) = usage.get(from).filter(|value| !value.is_null()) {
            subscription[to] = value.clone();
        }
    }
    Some(subscription)
}

pub fn credit_packs_from(usage: &Value) -> Vec<Value> {
    usage
        .get("resources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|item| {
            let mut row = Map::new();
            row.insert(
                "name".into(),
                item.get("name").cloned().unwrap_or(json!("")),
            );
            for (from, to) in [
                ("total", "total"),
                ("used", "used"),
                ("left", "remain"),
                ("createAt", "grantAt"),
                ("startAt", "startAt"),
                ("expireAt", "expireAt"),
                ("refreshAt", "refreshAt"),
                ("grantReason", "grantReason"),
            ] {
                if let Some(value) = item.get(from).filter(|value| !value.is_null()) {
                    row.insert(to.into(), value.clone());
                }
            }
            if item.get("isDaily").and_then(Value::as_bool) == Some(true) {
                row.insert("daily".into(), json!(true));
            }
            Value::Object(row)
        })
        .collect()
}
