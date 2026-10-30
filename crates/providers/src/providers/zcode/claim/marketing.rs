//! 营销活动只读发现与领取后的确认回执。
//!
//! 周末活动可仅出现在 /marketing/touch 的 claim_zcode_plan 按钮中，
//! billing/preview 为空不能排除这类套餐。只读取套餐元数据，不加载或执行营销资源。

use std::collections::BTreeMap;

use agent2api_provider_protocol::WireProxy;
use serde_json::{json, Value};

use crate::errors::GatewayError;
use crate::http::send_raw;

use super::plans::{identifier, integer, merge, merge_plan, parse_plan, ClaimablePlan};
use super::super::region::Region;
use super::{app_version, error_message, REQUEST_TIMEOUT_MS};

const QUERY_PATH: &str = "/api/v1/marketing/touch";
const REPORT_PATH: &str = "/api/v1/marketing/touch/action";
const MAX_DEPTH: usize = 12;
const MAX_NODES: usize = 4096;
const MAX_DELIVERIES: usize = 32;

fn headers(jwt: &str, device_mid: Option<&str>, version: &str) -> Vec<(String, String)> {
    let mut headers = vec![
        ("X-Client-Language".to_string(), "zh-CN".to_string()),
        ("X-ZCode-App-Version".to_string(), version.to_string()),
    ];
    if !jwt.trim().is_empty() {
        headers.push(("Authorization".to_string(), format!("Bearer {}", jwt.trim())));
    }
    if let Some(mid) = device_mid.map(str::trim).filter(|value| !value.is_empty()) {
        headers.push(("X-Device-Mid".to_string(), mid.to_string()));
    }
    headers
}

pub(super) async fn query(
    region: Region,
    jwt: &str,
    device_mid: Option<&str>,
    proxy: Option<&WireProxy>,
) -> Result<Vec<ClaimablePlan>, GatewayError> {
    let url = format!("{}{QUERY_PATH}?seq=0", region.zcode_origin());
    let version = app_version();
    let response = send_raw("GET", &url, None, &headers(jwt, device_mid, &version), proxy, Some(REQUEST_TIMEOUT_MS))
        .await
        .map_err(|error| GatewayError::with_status(
            if error.is_timeout() { 504 } else { 502 },
            format!("营销套餐查询失败：{error}"),
        ))?;
    if response.status == 404 { return Ok(Vec::new()); }
    let payload = response.payload
        .ok_or_else(|| GatewayError::with_status(502, "营销套餐查询：上游响应不是 JSON"))?;
    let code = payload.get("code").and_then(integer);
    if !(200..300).contains(&response.status) || code != Some(0)
        || payload.get("success").and_then(Value::as_bool) == Some(false)
    {
        return Err(GatewayError::with_status(
            if response.status == 401 || code == Some(401) { 401 } else { 502 },
            format!("营销套餐查询失败：{}", error_message(&payload, response.status)),
        ).with_optional_code(code));
    }
    let deliveries = payload.pointer("/data/deliveries").and_then(Value::as_array)
        .ok_or_else(|| GatewayError::with_status(502, "营销套餐查询：上游未返回有效的活动列表"))?;
    if deliveries.len() > MAX_DELIVERIES {
        return Err(GatewayError::with_status(502, "营销套餐查询：活动列表超过上限"));
    }
    let mut plans = Vec::new();
    for delivery in deliveries {
        let Some(campaign) = delivery.get("campaign_id").and_then(identifier) else { continue; };
        let mut metadata = BTreeMap::<String, ClaimablePlan>::new();
        let mut budget = MAX_NODES;
        collect_metadata(delivery, 0, &mut budget, &mut metadata);
        let priority = delivery.get("priority").and_then(integer).unwrap_or(0);
        let mut found = Vec::new();
        for path in ["/banner/buttons", "/banner/success_popup/buttons", "/popup/buttons"] {
            let Some(buttons) = delivery.pointer(path).and_then(Value::as_array) else { continue; };
            for button in buttons {
                let Some(action) = button.get("action") else { continue; };
                if action.get("type").and_then(Value::as_str) != Some("claim_zcode_plan") { continue; }
                let Some(plan_id) = action.pointer("/args/plan_id").and_then(identifier) else { continue; };
                let mut plan = metadata.get(plan_id).cloned().unwrap_or_else(|| ClaimablePlan {
                    plan_id: plan_id.to_string(),
                    name: plan_id.to_string(),
                    description: String::new(),
                    priority,
                    starts_at: None,
                    ends_at: None,
                    entitlements: Vec::new(),
                    marketing_campaign_ids: Vec::new(),
                });
                if plan.priority == 0 { plan.priority = priority; }
                plan.marketing_campaign_ids.push(campaign.to_string());
                found.push(plan);
            }
        }
        merge(&mut plans, found);
    }
    Ok(plans)
}

fn collect_metadata(
    node: &Value,
    depth: usize,
    budget: &mut usize,
    metadata: &mut BTreeMap<String, ClaimablePlan>,
) {
    if depth > MAX_DEPTH || *budget == 0 { return; }
    *budget -= 1;
    match node {
        Value::Array(items) => {
            for item in items {
                collect_metadata(item, depth + 1, budget, metadata);
                if *budget == 0 { break; }
            }
        }
        Value::Object(fields) => {
            if let Some(plan) = parse_plan(node) {
                match metadata.get_mut(&plan.plan_id) {
                    Some(existing) => merge_plan(existing, plan),
                    None => { metadata.insert(plan.plan_id.clone(), plan); }
                }
            }
            for value in fields.values() {
                collect_metadata(value, depth + 1, budget, metadata);
                if *budget == 0 { break; }
            }
        }
        _ => {}
    }
}

pub(super) async fn confirm(
    region: Region,
    jwt: &str,
    device_mid: Option<&str>,
    proxy: Option<&WireProxy>,
    campaign_id: &str,
) -> Result<(), String> {
    let url = format!("{}{REPORT_PATH}", region.zcode_origin());
    let version = app_version();
    let body = json!({ "campaign_id": campaign_id, "action_type": "confirm" });
    // 确认 POST 结果不确定时不自动重发，也不以 cancel 代替失败。
    let response = send_raw("POST", &url, Some(&body), &headers(jwt, device_mid, &version), proxy, Some(REQUEST_TIMEOUT_MS))
        .await.map_err(|error| format!("营销回执失败：{error}"))?;
    let payload = response.payload.ok_or_else(|| "营销回执响应不是 JSON".to_string())?;
    if !(200..300).contains(&response.status) || payload.get("code").and_then(integer) != Some(0)
        || payload.get("success").and_then(Value::as_bool) == Some(false)
    {
        return Err(format!("营销回执失败：{}", error_message(&payload, response.status)));
    }
    Ok(())
}
