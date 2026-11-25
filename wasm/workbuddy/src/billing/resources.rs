//! 国内新版summary/paid/free积分接口；分页不完整返回未知，不能伪装零余额。
use super::commodity::codes;
use crate::{session::Session, util};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

const PAID: &[&str] = &[
    codes::PRO_MON,
    codes::PRO_MON_PLUS,
    codes::PRO_YEAR,
    codes::YOUTH,
    codes::ADVANCED,
    codes::FLAGSHIP,
    codes::EXTRA,
    codes::EXTRA38,
];
const FREE: &[&str] = &[
    codes::FREE,
    codes::FREE_MON,
    codes::GIFT,
    codes::PRO_TRIAL_MON,
    codes::PRO_TRIAL_YEAR,
    codes::ACTIVITY,
    codes::BONUS28,
    codes::BONUS29,
    codes::BONUS30,
];
pub struct Resources {
    pub accounts: Vec<Value>,
    pub summary: Option<(f64, f64)>,
}
fn should_abort_summary(error: &Error) -> bool {
    error.status.is_none()
        || matches!(error.status, Some(401 | 408 | 504))
        || matches!(
            error.code.as_deref(),
            Some(
                "timeout"
                    | "usage_timeout"
                    | "aborted"
                    | "forbidden"
                    | "plugin_busy"
                    | "plugin_resource_limit"
                    | "plugin_unavailable"
            )
        )
}

pub fn fetch(current: &Session) -> Result<Resources> {
    let summary = match summary(current) {
        Ok(summary) => summary,
        Err(error) if should_abort_summary(&error) => return Err(error),
        Err(_) => None,
    };
    let day = util::day(sdk::now());
    let paid = pages(
        current,
        "/billing/meter/get-user-resource-paid-packages",
        PAID,
        &json!({ "NeedRenewInfo": true }),
    )?;
    let free = pages(
        current,
        "/billing/meter/get-user-resource-free-packages",
        FREE,
        &json!({ "SlicePeriodStartTime": format!("{day} 00:00:00"), "SlicePeriodEndTime": format!("{day} 23:59:59") }),
    )?;
    let mut seen = std::collections::HashSet::new();
    let mut accounts = Vec::new();
    for row in paid.into_iter().chain(free) {
        if row
            .get("ResourceId")
            .filter(|v| !v.is_null())
            .map(Value::to_string)
            .filter(|v| v != "\"\"")
            .is_some_and(|id| !seen.insert(id))
        {
            continue;
        }
        accounts.push(row);
    }
    Ok(Resources { accounts, summary })
}
fn data(value: &Value) -> &Value {
    value
        .pointer("/Response/Data")
        .or_else(|| value.pointer("/Response"))
        .or_else(|| value.get("data"))
        .unwrap_or(value)
}
fn summary(current: &Session) -> Result<Option<(f64, f64)>> {
    let result = super::request(
        current,
        "POST",
        "/billing/meter/get-user-resource-summary",
        None,
    )?;
    let Some(rows) = data(&result)["Packages"].as_array() else {
        return Ok(None);
    };
    let mut total = 0.0;
    let mut left = 0.0;
    for row in rows {
        let (Some(size), Some(remain)) = (
            number(row.get("CycleTotalCapacity")),
            number(row.get("CycleRemainCapacity")),
        ) else {
            return Ok(None);
        };
        total += size;
        left += remain.min(size);
    }
    Ok(Some((total, left)))
}
fn pages(current: &Session, path: &str, codes: &[&str], extra: &Value) -> Result<Vec<Value>> {
    let mut accounts = Vec::new();
    for page in 1..=10 {
        let mut body =
            json!({ "PageNumber": page, "PageSize": 200, "PackageCodes": codes, "Status": [0,3] });
        if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            body.extend(extra.clone());
        }
        let result = super::request(current, "POST", path, Some(&body))?;
        let data = data(&result);
        let rows = data["Accounts"]
            .as_array()
            .ok_or_else(|| Error::http(502, "积分包缺少Accounts，余额未知"))?;
        if rows.len() > 200 {
            return Err(Error::http(502, "积分包超过页长，余额未知"));
        }
        let total = match data.get("TotalCount") {
            None | Some(Value::Null) => None,
            value => Some(
                number(value)
                    .filter(|v| v.fract() == 0.0 && *v <= 2000.0)
                    .ok_or_else(|| Error::http(502, "积分包总数无效或过大"))?
                    as usize,
            ),
        };
        accounts.extend(rows.iter().cloned());
        if total.is_some_and(|total| accounts.len() > total) {
            return Err(Error::http(502, "积分包页数与总数不一致，余额未知"));
        }
        if total == Some(accounts.len()) {
            return Ok(accounts);
        }
        if rows.len() < 200 {
            if total.is_some_and(|total| accounts.len() < total) {
                return Err(Error::http(502, "积分包分页未完成，余额未知"));
            }
            return Ok(accounts);
        }
    }
    Err(Error::http(502, "积分包超过10页护栏，未使用不完整余额"))
}
pub fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(util::number).filter(|v| *v >= 0.0)
}
pub fn capacity(resource: &Value) -> (Option<f64>, Option<f64>) {
    let detail = (resource["CapacityType"] == 4)
        .then(|| {
            resource["SlicePeriodUsageDetails"]
                .as_array()
                .and_then(|v| v.first())
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
