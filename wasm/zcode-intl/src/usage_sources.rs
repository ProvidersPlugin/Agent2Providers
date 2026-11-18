//! 从本家已确认的原始账单字段投影额度来源，原始凭据/票据不进入返回文档。
use agent2api_wasm_sdk::{json, Value};
use super::network::{number, text};

fn milliseconds(value: &Value) -> i64 {
    number(value).filter(|value| *value > 0.0).map(|value| if value > 1e11 { value as i64 } else { (value * 1000.0) as i64 }).unwrap_or(0)
}
fn bound(value: &Value, fields: &[&str]) -> i64 { fields.iter().map(|key| milliseconds(&value[*key])).find(|value| *value > 0).unwrap_or(0) }
fn source(id: &str, label: &str, unit: &str, remaining: Option<f64>, total: Option<f64>, used: Option<f64>) -> Value {
    json!({ "id": id, "label": label, "provider": super::network::PROVIDER, "unit": unit, "remaining": remaining, "total": total, "used": used })
}
fn owner(plan: &Value, bucket: &Value) -> bool {
    let a = text(plan, &["user_plan_id"]); let b = text(bucket, &["user_plan_id"]);
    if !a.is_empty() && !b.is_empty() { a == b } else { !text(plan, &["plan_id"]).is_empty() && plan["plan_id"] == bucket["plan_id"] }
}
fn entitlement(a: &Value, b: &Value) -> bool {
    let left = text(a, &["entitlement_id"]); let right = text(b, &["entitlement_id"]);
    if !left.is_empty() && !right.is_empty() { left == right } else { !text(a, &["show_name"]).is_empty() && a["show_name"] == b["show_name"] }
}
fn models(bucket: Option<&Value>, entitlement: &Value, label: &str) -> Vec<String> {
    let mut output = Vec::new();
    for value in bucket.into_iter().chain(std::iter::once(entitlement)) {
        for capability in value["capabilities"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            let capability = capability.trim();
            if capability.get(..6).is_some_and(|prefix| prefix.eq_ignore_ascii_case("model:")) {
                let model = capability[6..].trim().to_ascii_lowercase();
                if !model.is_empty() && !output.contains(&model) { output.push(model); }
            }
        }
    }
    if output.is_empty() { let model = label.trim().to_ascii_lowercase().replace(' ', "-"); if model.starts_with("glm-") { output.push(model); } }
    output
}

pub fn billing(data: &Value) -> Vec<Value> {
    let balances = data["balances"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let plans = data["plans"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let mut matched = vec![false; balances.len()];
    let mut output = Vec::new();
    for plan in plans {
        let instance = text(plan, &["user_plan_id", "plan_id"]);
        if instance.is_empty() { continue; }
        for item in plan["entitlements"].as_array().into_iter().flatten() {
            let buckets = balances.iter().enumerate().filter(|(_, bucket)| owner(plan, bucket) && entitlement(item, bucket)).collect::<Vec<_>>();
            if buckets.is_empty() { append(plan, item, None, &instance, false, &mut output); }
            else {
                let split = buckets.len() > 1;
                for (index, bucket) in buckets { matched[index] = true; append(plan, item, Some(bucket), &instance, split, &mut output); }
            }
        }
    }
    for (index, bucket) in balances.iter().enumerate() {
        if matched[index] { continue; }
        let plan = plans.iter().find(|plan| owner(plan, bucket)).unwrap_or(&Value::Null);
        let instance = text(plan, &["user_plan_id", "plan_id"]);
        let instance = if instance.is_empty() { text(bucket, &["user_plan_id", "plan_id"]) } else { instance };
        append(plan, &Value::Null, Some(bucket), &instance, false, &mut output);
    }
    output
}

fn append(plan: &Value, entitlement: &Value, bucket: Option<&Value>, instance: &str, split: bool, output: &mut Vec<Value>) {
    let data = bucket.unwrap_or(entitlement);
    let label = text(data, &["show_name"]); let label = if label.is_empty() { text(entitlement, &["show_name"]) } else { label };
    if label.is_empty() { return; }
    let read = |key: &str| bucket.and_then(|value| number(&value[key])).or_else(|| number(&entitlement[key]));
    let total = read("total_units").or_else(|| read("grant_units"));
    let used = read("used_units");
    let remaining = read("remaining_units").or_else(|| total.zip(used).map(|(total, used)| total - used)).or_else(|| read("available_units"));
    let identity = text(entitlement, &["entitlement_id"]);
    let identity = if identity.is_empty() { text(data, &["entitlement_id", "show_name"]) } else { identity };
    let mut id = format!("plan:{instance}:{identity}");
    if split && !text(data, &["bucket_id"]).is_empty() { id.push_str(&format!(":{}", text(data, &["bucket_id"]))); }
    let unit = text(data, &["unit_type"]); let unit = if unit.is_empty() { text(entitlement, &["unit_type"]) } else { unit };
    let mut row = source(&id, &label, &unit, remaining, total, used);
    let start = milliseconds(&entitlement["effective_at"]);
    let start = if start > 0 { start } else { bound(plan, &["starts_at", "start_at", "startsAt"]) };
    let end = bound(plan, &["ends_at", "end_at", "endsAt", "expire_at", "expires_at"]);
    let expiry = milliseconds(&data["expires_at"]);
    let end = match (end, expiry) { (0, expiry) => expiry, (end, 0) => end, (end, expiry) => end.min(expiry) };
    let period_start = milliseconds(&data["period_start"]); let period_end = milliseconds(&data["period_end"]);
    let period = text(entitlement, &["period"]).to_ascii_lowercase();
    row["channel"] = json!("start-plan"); row["planId"] = json!(instance); row["planName"] = json!(text(plan, &["name", "plan_id"]));
    row["models"] = json!(models(bucket, entitlement, &label)); row["startsAt"] = json!(start); row["endsAt"] = json!(end); row["resetsAt"] = json!(period_end);
    row["window"] = if matches!(period.as_str(), "daily" | "weekly" | "monthly") {
        json!({ "reset": period, "utcOffsetMinutes": 480, "startsAt": start, "endsAt": end })
    } else if period_start > 0 && period_end > period_start { json!({ "reset": "total", "startsAt": period_start.max(start), "endsAt": period_end }) }
        else { json!({ "reset": "total", "startsAt": start, "endsAt": end }) };
    row["localEstimate"] = json!(used.is_none() && remaining.is_none()); row["unlimited"] = json!(data["unlimited"] == true);
    if !output.iter().any(|row| row["id"] == id) { output.push(row); }
}

pub fn monitor(data: &Value) -> Vec<Value> {
    data["limits"].as_array().into_iter().flatten().map(|limit| {
        let kind = text(limit, &["type"]); let unit = number(&limit["unit"]).unwrap_or(0.0) as i64;
        let period = number(&limit["number"]).unwrap_or(1.0);
        let label = match unit { 3 => format!("{period} 小时窗口"), 4 => "每日窗口".into(), 5 => "每月窗口".into(), 6 => "每周窗口".into(), _ => format!("额度窗口 {unit}") };
        let total = number(&limit["usage"]); let used = number(&limit["currentValue"]);
        let remaining = number(&limit["remaining"]).or_else(|| total.zip(used).map(|(total, used)| total - used));
        let mut row = source(&format!("coding:{kind}:{unit}:{period}"), &format!("{kind} · {label}"), "", remaining, total, used);
        row["channel"] = json!("coding-plan"); row["remainingPercent"] = json!(number(&limit["percentage"]).map(|used| (100.0 - used).clamp(0.0, 100.0)));
        row["resetsAt"] = json!(milliseconds(&limit["nextResetTime"])); row
    }).collect()
}
