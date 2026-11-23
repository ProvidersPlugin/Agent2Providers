use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use crate::errors::GatewayError;
use super::{control, credentials::OfficeAceCredential, oauth, signer::Algorithm, upstream};

pub const SKU: &str = "officeace.personal.package.trial";
const RESOURCE_TYPE: &str = "hws.resource.type.agentarts.platform";
const RESOURCES: &str = "/v1/subscription/resources?type=OfficeAce&is_count_down_info=true&is_subscribed=true";
const PRICING: &str = "/v1/skus/officeace.personal.package.trial?is_query_resource_rating=true&is_query_sales_cycle=true&period_type=2&period_num=1&product_num=1";

pub async fn resources(
    ctx: &PluginCtx,
    credential: &mut OfficeAceCredential,
) -> Result<Vec<Value>, GatewayError> {
    let value = control::cloud_call(ctx, credential, "GET", RESOURCES, None).await?;
    resource_rows(value)
}

pub async fn resources_readonly(credential: &OfficeAceCredential) -> Result<Vec<Value>, GatewayError> {
    let value = upstream::call(
        credential, "GET", &format!("{}{RESOURCES}", oauth::CLOUD_BASE), Algorithm::Sdk, None, &[],
    ).await?;
    resource_rows(value)
}

fn resource_rows(value: Value) -> Result<Vec<Value>, GatewayError> {
    value.as_array().cloned().ok_or_else(|| GatewayError::with_status(
        502, "官方未返回有效的套餐订单清单，暂不能确认是否可开通",
    ))
}

pub async fn zero_price_order(
    ctx: &PluginCtx,
    credential: &mut OfficeAceCredential,
) -> Result<Value, GatewayError> {
    let offer = control::cloud_call(ctx, credential, "GET", PRICING, None).await?;
    let rating = offer.get("resource_rating").unwrap_or(&Value::Null);
    if offer.get("sku_code").and_then(Value::as_str) != Some(SKU)
        || offer.get("status").and_then(Value::as_str) != Some("active")
        || offer.get("purchase_model").and_then(Value::as_str) != Some("PERIOD")
        || offer.get("sub_product_code").and_then(Value::as_str) != Some(RESOURCE_TYPE)
        || rating.get("currency").and_then(Value::as_str) != Some("CNY")
        || !rating.get("amount").is_some_and(is_zero)
    {
        return Err(GatewayError::with_status(409, "官方未确认个人体验套餐为零元，已停止开通；不会购买付费套餐"));
    }
    let cycles: Vec<&Value> = offer.get("product_sales_cycles").and_then(Value::as_array)
        .into_iter().flatten().filter(|cycle| {
            cycle.get("status").and_then(Value::as_str) == Some("ONSALE")
                && cycle.get("resourceSpecCode").and_then(Value::as_str) == Some(SKU)
                && cycle.get("resourceType").and_then(Value::as_str) == Some(RESOURCE_TYPE)
                && cycle.get("regionList").and_then(Value::as_array).is_some_and(|regions| {
                    regions.iter().any(|region| region.get("regionCode").and_then(Value::as_str) == Some(oauth::REGION))
                })
                && cycle.get("periodList").and_then(Value::as_array).is_some_and(|periods| {
                    periods.iter().any(|period| period.get("periodType").and_then(Value::as_str) == Some("MONTH")
                        && period.get("periodValueList").and_then(Value::as_array).is_some_and(|values| {
                            values.iter().any(|value| is_one(&value["value"]))
                        }))
                })
        }).collect();
    if cycles.len() != 1 {
        return Err(GatewayError::with_status(409, "未找到唯一的零元一个月体验套餐，已停止开通"));
    }
    let atomic = cycles[0].get("id").and_then(Value::as_str).filter(|id| !id.trim().is_empty())
        .ok_or_else(|| GatewayError::with_status(502, "官方未返回体验套餐的产品标识，未提交订单"))?;
    Ok(json!({
        "service_type": "hws.service.type.agentarts",
        "subscribe_period_type": 2,
        "subscribe_period_value": 1,
        "auto_renewal": 0,
        "auto_pay": 1,
        "subscribe_sku_info": {
            "subscribe_quantity": 1,
            "subscribe_sku_code": SKU,
            "subscribe_sub_product_code": RESOURCE_TYPE,
            "subscribe_atomic_product_id": atomic,
        },
        "agreement_list": [{
            "agree_declaration": 1,
            "version": "1.0",
            "privacy_statement": "/declaration/agentarts.html",
        }],
    }))
}

fn is_zero(value: &Value) -> bool {
    if let Some(number) = value.as_number() {
        return number.to_string().chars().all(|ch| matches!(ch, '0' | '.'));
    }
    value.as_str().is_some_and(|text| {
        let text = text.trim();
        !text.is_empty() && text.chars().any(|ch| ch == '0')
            && text.chars().all(|ch| matches!(ch, '0' | '.'))
            && text.matches('.').count() <= 1
    })
}

fn is_one(value: &Value) -> bool {
    value.as_str() == Some("1") || value.as_u64() == Some(1)
}

pub fn no_auto_renew(rows: &[Value]) -> Option<bool> {
    let trials: Vec<&Value> = rows.iter().filter(|row| row.get("sku_code").and_then(Value::as_str) == Some(SKU)
        && row.get("status").and_then(Value::as_str) == Some("SUBSCRIBED")).collect();
    if trials.is_empty() { return None; }
    let mut known = false;
    for row in trials {
        match row.get("is_auto_renew") {
            Some(value) if is_zero(value) => known = true,
            Some(value) if is_one(value) => return Some(false),
            _ => return None,
        }
    }
    known.then_some(true)
}

pub fn has_open_order(rows: &[Value]) -> bool {
    rows.iter().any(|row| !matches!(row.get("status").and_then(Value::as_str),
        Some("FAILED" | "CANCELLED" | "CANCELED" | "UNSUBSCRIBED" | "EXPIRED" | "DELETED")))
}
