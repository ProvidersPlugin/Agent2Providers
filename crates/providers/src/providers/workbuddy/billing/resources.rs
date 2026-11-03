//! 新版积分接口按 UTC+8 查询当天切片；缺字段和未完成分页都不能当成零余额。

use std::collections::HashSet;

use agent2api_provider_protocol::WireProxy;
use chrono::{FixedOffset, Utc};
use serde_json::{json, Value};

use crate::errors::GatewayError;

use super::{call, commodity::codes};

const PAGE_SIZE: usize = 200;
const MAX_PAGES: usize = 10;
const PAID: &[&str] = &[
    codes::PRO_MON,
    codes::PRO_MON_PLUS,
    codes::PRO_YEAR,
    codes::YOUTH,
    codes::ADVANCED,
    codes::FLAGSHIP,
    codes::EXTRA,
    codes::EXTRA38,
    codes::EXTRA_INTL,
];
const FREE: &[&str] = &[
    codes::FREE,
    codes::FREE_MON,
    codes::FREE_MON_INTL,
    codes::GIFT,
    codes::PRO_TRIAL_MON,
    codes::PRO_TRIAL_YEAR,
    codes::ACTIVITY,
    codes::BONUS28,
    codes::BONUS_INTL,
    codes::BONUS29,
    codes::BONUS30,
];

pub(super) struct Resources {
    pub accounts: Vec<Value>,
    pub summary: Option<(f64, f64)>,
}

pub(super) async fn fetch(
    session: &Value,
    proxy: Option<&WireProxy>,
    locale: Option<&str>,
) -> Result<Resources, GatewayError> {
    let locale = locale.unwrap_or_else(|| {
        if session.get("edition").and_then(Value::as_str) == Some("intl") {
            "en"
        } else {
            "zh"
        }
    });
    let now = Utc::now();
    let day = FixedOffset::east_opt(8 * 3600)
        .map(|offset| now.with_timezone(&offset).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| now.format("%Y-%m-%d").to_string());
    let paid = json!({ "NeedRenewInfo": true });
    let free = json!({
        "SlicePeriodStartTime": format!("{day} 00:00:00"),
        "SlicePeriodEndTime": format!("{day} 23:59:59"),
    });
    let (summary, paid, free) = tokio::join!(
        summary(session, proxy, locale),
        pages(
            session,
            proxy,
            locale,
            "/billing/meter/get-user-resource-paid-packages",
            PAID,
            &paid
        ),
        pages(
            session,
            proxy,
            locale,
            "/billing/meter/get-user-resource-free-packages",
            FREE,
            &free
        ),
    );
    let mut seen = HashSet::new();
    let mut accounts = Vec::new();
    for account in paid?.into_iter().chain(free?) {
        let id = account
            .get("ResourceId")
            .filter(|value| !value.is_null())
            .map(Value::to_string)
            .filter(|value| value != "\"\"");
        if id.is_some_and(|id| !seen.insert(id)) {
            continue;
        }
        accounts.push(account);
    }
    Ok(Resources {
        accounts,
        summary: summary.ok().flatten(),
    })
}

async fn summary(
    session: &Value,
    proxy: Option<&WireProxy>,
    locale: &str,
) -> Result<Option<(f64, f64)>, GatewayError> {
    let body = json!({});
    let result = call::call(
        "POST",
        "/billing/meter/get-user-resource-summary",
        Some(&body),
        session,
        proxy,
        Some(locale),
        call::Options::default(),
    )
    .await?;
    let Some(packages) = data(&result.data).get("Packages").and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut total = 0.0;
    let mut left = 0.0;
    for package in packages {
        let (Some(size), Some(remain)) = (
            number(package.get("CycleTotalCapacity")),
            number(package.get("CycleRemainCapacity")),
        ) else {
            return Ok(None);
        };
        total += size;
        left += remain.min(size);
    }
    Ok(Some((total, left)))
}

async fn pages(
    session: &Value,
    proxy: Option<&WireProxy>,
    locale: &str,
    path: &str,
    codes: &[&str],
    extra: &Value,
) -> Result<Vec<Value>, GatewayError> {
    let mut accounts = Vec::new();
    for page in 1..=MAX_PAGES {
        let mut body = json!({
            "PageNumber": page, "PageSize": PAGE_SIZE,
            "PackageCodes": codes, "Status": [0, 3],
        });
        if let (Some(fields), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            fields.extend(extra.clone());
        }
        let result = call::call(
            "POST",
            path,
            Some(&body),
            session,
            proxy,
            Some(locale),
            call::Options::default(),
        )
        .await?;
        let data = data(&result.data);
        let rows = data
            .get("Accounts")
            .and_then(Value::as_array)
            .ok_or_else(|| GatewayError::with_status(502, "积分包接口缺少 Accounts，余额未知"))?;
        let count = rows.len();
        if count > PAGE_SIZE {
            return Err(GatewayError::with_status(
                502,
                "积分包分页超过请求页长，余额未知",
            ));
        }
        let total = match data.get("TotalCount") {
            None | Some(Value::Null) => None,
            value => Some(
                number(value)
                    .filter(|value| {
                        value.fract() == 0.0 && *value <= (PAGE_SIZE * MAX_PAGES) as f64
                    })
                    .ok_or_else(|| {
                        GatewayError::with_status(502, "积分包总数无效或超过分页上限，余额未知")
                    })? as usize,
            ),
        };
        accounts.extend(rows.iter().cloned());
        if total.is_some_and(|total| accounts.len() >= total) {
            return Ok(accounts);
        }
        if count < PAGE_SIZE {
            if total.is_some_and(|total| accounts.len() < total) {
                return Err(GatewayError::with_status(
                    502,
                    "积分包分页数量不完整，余额未知",
                ));
            }
            return Ok(accounts);
        }
    }
    Err(GatewayError::with_status(
        502,
        "积分包超过分页上限，未将不完整列表作为余额",
    ))
}

fn data(value: &Value) -> &Value {
    value
        .pointer("/Response/Data")
        .or_else(|| value.pointer("/Response"))
        .or_else(|| value.get("data"))
        .unwrap_or(value)
}

pub(super) fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(value) => value.as_f64(),
        Value::String(value) if !value.trim().is_empty() => value.trim().parse().ok(),
        _ => None,
    }
    .filter(|value: &f64| value.is_finite() && *value >= 0.0)
}

pub(super) fn capacity(resource: &Value) -> (Option<f64>, Option<f64>) {
    let slice = resource.get("CapacityType").and_then(Value::as_i64) == Some(4);
    let detail = slice
        .then(|| {
            resource
                .get("SlicePeriodUsageDetails")
                .and_then(Value::as_array)
                .and_then(|details| details.first())
        })
        .flatten();
    let (total, left) = if let Some(detail) = detail {
        (
            number(detail.get("SlicePeriodCapacitySizePrecise")),
            number(detail.get("SlicePeriodCapacityRemainPrecise")),
        )
    } else {
        (
            number(resource.get("CycleCapacitySizePrecise")),
            number(resource.get("CycleCapacityRemainPrecise")),
        )
    };
    (
        total,
        left.map(|left| total.map(|total| left.min(total)).unwrap_or(left)),
    )
}
