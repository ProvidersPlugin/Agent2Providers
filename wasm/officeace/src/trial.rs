//! 旧零元体验套餐规则逐项保留：SKU/币种/单价/区域/月数唯一匹配，显式auto_renewal=0。
//! 只查询和使用已有项目，不创建云资源。状态不明、历史订单存在均不自动购买。

use super::{client, control::Context, credentials::Credentials, endpoints, signer::Algorithm};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub const SKU: &str = "officeace.personal.package.trial";
const TYPE: &str = "hws.resource.type.agentarts.platform";
const RESOURCES: &str =
    "/v1/subscription/resources?type=OfficeAce&is_count_down_info=true&is_subscribed=true";
const PRICING: &str = "/v1/skus/officeace.personal.package.trial?is_query_resource_rating=true&is_query_sales_cycle=true&period_type=2&period_num=1&product_num=1";

fn rows(value: Value) -> Result<Vec<Value>> {
    value
        .as_array()
        .cloned()
        .ok_or_else(|| Error::http(502, "官方套餐订单清单无效，无法确认是否可开通"))
}
pub fn resources(context: &mut Context) -> Result<Vec<Value>> {
    rows(context.cloud("GET", RESOURCES, None)?)
}
pub fn readonly(credential: &Credentials) -> Result<Vec<Value>> {
    rows(client::signed(
        credential,
        "GET",
        &format!("{}{RESOURCES}", endpoints::CLOUD),
        Algorithm::Sdk,
        None,
        json!({}),
    )?)
}

pub fn zero_order(context: &mut Context) -> Result<Value> {
    let offer = context.cloud("GET", PRICING, None)?;
    let rating = &offer["resource_rating"];
    if offer["sku_code"] != SKU
        || offer["status"] != "active"
        || offer["purchase_model"] != "PERIOD"
        || offer["sub_product_code"] != TYPE
        || rating["currency"] != "CNY"
        || !zero(&rating["amount"])
    {
        return Err(Error::http(
            409,
            "官方未确认个人体验套餐为零元，已停止开通；不会购买付费套餐",
        ));
    }
    let cycles: Vec<&Value> = offer["product_sales_cycles"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|cycle| {
            cycle["status"] == "ONSALE"
                && cycle["resourceSpecCode"] == SKU
                && cycle["resourceType"] == TYPE
                && cycle["regionList"].as_array().is_some_and(|regions| {
                    regions
                        .iter()
                        .any(|region| region["regionCode"] == endpoints::REGION)
                })
                && cycle["periodList"].as_array().is_some_and(|periods| {
                    periods.iter().any(|period| {
                        period["periodType"] == "MONTH"
                            && period["periodValueList"].as_array().is_some_and(|values| {
                                values.iter().any(|value| one(&value["value"]))
                            })
                    })
                })
        })
        .collect();
    if cycles.len() != 1 {
        return Err(Error::http(
            409,
            "未找到唯一的零元一个月体验套餐，已停止开通",
        ));
    }
    let id = cycles[0]["id"]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| Error::http(502, "官方没有返回体验套餐产品标识，未提交订单"))?;
    Ok(
        json!({ "service_type": "hws.service.type.agentarts", "subscribe_period_type": 2,
        "subscribe_period_value": 1, "auto_renewal": 0, "auto_pay": 1,
        "subscribe_sku_info": { "subscribe_quantity": 1, "subscribe_sku_code": SKU,
            "subscribe_sub_product_code": TYPE, "subscribe_atomic_product_id": id },
        "agreement_list": [{ "agree_declaration": 1, "version": "1.0", "privacy_statement": "/declaration/agentarts.html" }] }),
    )
}

fn zero(value: &Value) -> bool {
    let text = value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_number().map(ToString::to_string))
        .unwrap_or_default();
    let text = text.trim();
    !text.is_empty()
        && text.chars().any(|ch| ch == '0')
        && text.chars().all(|ch| matches!(ch, '0' | '.'))
        && text.matches('.').count() <= 1
}
fn one(value: &Value) -> bool {
    value.as_u64() == Some(1) || value.as_str() == Some("1")
}

pub fn no_renew(rows: &[Value]) -> Option<bool> {
    let trials: Vec<&Value> = rows
        .iter()
        .filter(|row| row["sku_code"] == SKU && row["status"] == "SUBSCRIBED")
        .collect();
    if trials.is_empty() {
        return None;
    }
    for trial in trials {
        if one(&trial["is_auto_renew"]) {
            return Some(false);
        }
        if !zero(&trial["is_auto_renew"]) {
            return None;
        }
    }
    Some(true)
}

pub fn has_order(rows: &[Value]) -> bool {
    rows.iter().any(|row| {
        !matches!(
            row["status"].as_str(),
            Some("FAILED" | "CANCELLED" | "CANCELED" | "UNSUBSCRIBED" | "EXPIRED" | "DELETED")
        )
    })
}
