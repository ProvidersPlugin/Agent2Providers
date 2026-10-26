use serde_json::{json, Value};

use super::{number, source, text};

fn millis(value: Option<&Value>) -> i64 {
    number(value).filter(|value| *value > 0.0)
        .map(|value| if value > 1e11 { value as i64 } else { (value * 1000.0) as i64 }).unwrap_or(0)
}

fn bound(value: &Value, keys: &[&str]) -> i64 {
    keys.iter().map(|key| millis(value.get(*key))).find(|value| *value > 0).unwrap_or(0)
}

fn owner_matches(plan: &Value, bucket: &Value) -> bool {
    let plan_instance = text(plan, "user_plan_id");
    let bucket_instance = text(bucket, "user_plan_id");
    if !plan_instance.is_empty() && !bucket_instance.is_empty() { plan_instance == bucket_instance }
    else { !text(plan, "plan_id").is_empty() && text(plan, "plan_id") == text(bucket, "plan_id") }
}

fn entitlement_matches(entitlement: &Value, bucket: &Value) -> bool {
    let left = text(entitlement, "entitlement_id");
    let right = text(bucket, "entitlement_id");
    if !left.is_empty() && !right.is_empty() { left == right }
    else { !text(entitlement, "show_name").is_empty() && text(entitlement, "show_name") == text(bucket, "show_name") }
}

fn models_for(bucket: Option<&Value>, entitlement: &Value, label: &str) -> Vec<String> {
    let mut models = Vec::new();
    for value in bucket.into_iter().chain(std::iter::once(entitlement)) {
        for capability in value.get("capabilities").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            let capability = capability.trim();
            if capability.len() >= 6 && capability.get(..6).is_some_and(|prefix| prefix.eq_ignore_ascii_case("model:")) {
                let model = capability[6..].trim().to_ascii_lowercase();
                if !model.is_empty() && !models.contains(&model) { models.push(model); }
            }
        }
    }
    if models.is_empty() {
        let model = label.trim().to_ascii_lowercase().replace(' ', "-");
        if model.starts_with("glm-") { models.push(model); }
    }
    models
}

pub(super) fn append(provider: &str, report: &Value, output: &mut Vec<Value>) {
    let raw = report.get("raw").unwrap_or(&Value::Null);
    let balances = raw.get("balances").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let mut matched = vec![false; balances.len()];
    for plan in raw.get("plans").and_then(Value::as_array).into_iter().flatten() {
        let instance = if text(plan, "user_plan_id").is_empty() { text(plan, "plan_id") } else { text(plan, "user_plan_id") };
        if instance.is_empty() { continue; }
        for entitlement in plan.get("entitlements").and_then(Value::as_array).into_iter().flatten() {
            let buckets: Vec<usize> = balances.iter().enumerate().filter_map(|(index, bucket)| {
                (owner_matches(plan, bucket) && entitlement_matches(entitlement, bucket)).then_some(index)
            }).collect();
            if buckets.is_empty() {
                append_plan(provider, plan, entitlement, None, instance, false, output);
            } else {
                let split = buckets.len() > 1;
                for index in buckets {
                    matched[index] = true;
                    append_plan(provider, plan, entitlement, Some(&balances[index]), instance, split, output);
                }
            }
        }
    }
    for (index, bucket) in balances.iter().enumerate() {
        if matched[index] { continue; }
        let plan = raw.get("plans").and_then(Value::as_array).into_iter().flatten().find(|plan| owner_matches(plan, bucket));
        let instance = plan.map(|plan| if text(plan, "user_plan_id").is_empty() { text(plan, "plan_id") } else { text(plan, "user_plan_id") })
            .unwrap_or_else(|| if text(bucket, "user_plan_id").is_empty() { text(bucket, "plan_id") } else { text(bucket, "user_plan_id") });
        append_plan(provider, plan.unwrap_or(&Value::Null), &Value::Null, Some(bucket), instance, false, output);
    }
    append_monitor(provider, report.get("quotaMonitorRaw").unwrap_or(raw), output);
}

fn append_plan(provider: &str, plan: &Value, entitlement: &Value, bucket: Option<&Value>, instance: &str, split: bool, output: &mut Vec<Value>) {
    let data = bucket.unwrap_or(entitlement);
    let label = if text(data, "show_name").is_empty() { text(entitlement, "show_name") } else { text(data, "show_name") };
    if label.is_empty() { return; }
    let read = |key: &str| bucket.and_then(|bucket| number(bucket.get(key))).or_else(|| number(entitlement.get(key)));
    let total = read("total_units").or_else(|| read("grant_units"));
    let used = read("used_units");
    let remaining = read("remaining_units").or_else(|| Some(total? - used?)).or_else(|| read("available_units"));
    let identity = if text(entitlement, "entitlement_id").is_empty() {
        if text(data, "entitlement_id").is_empty() { label } else { text(data, "entitlement_id") }
    } else { text(entitlement, "entitlement_id") };
    let mut id = format!("plan:{instance}:{identity}");
    if split && !text(data, "bucket_id").is_empty() { id.push_str(&format!(":{}", text(data, "bucket_id"))); }
    let unit = if text(data, "unit_type").is_empty() { text(entitlement, "unit_type") } else { text(data, "unit_type") };
    let mut row = source(provider, &id, label, unit, remaining, total, used);
    let effective = millis(entitlement.get("effective_at"));
    let start = if effective > 0 { effective } else { bound(plan, &["starts_at", "start_at", "startsAt"]) };
    let end = bound(plan, &["ends_at", "end_at", "endsAt", "expire_at", "expires_at"]);
    let expiry = bound(data, &["expires_at"]);
    let end = match (end, expiry) { (0, expiry) => expiry, (end, 0) => end, (end, expiry) => end.min(expiry) };
    let period_start = bound(data, &["period_start"]);
    let period_end = bound(data, &["period_end"]);
    row["channel"] = json!("start-plan");
    row["planId"] = json!(instance);
    row["planName"] = json!(if text(plan, "name").is_empty() { text(plan, "plan_id") } else { text(plan, "name") });
    row["models"] = json!(models_for(bucket, entitlement, label));
    row["startsAt"] = json!(start);
    row["endsAt"] = json!(end);
    row["resetsAt"] = json!(period_end);
    row["window"] = match text(entitlement, "period").to_ascii_lowercase().as_str() {
        "daily" | "weekly" | "monthly" => json!({ "reset": text(entitlement, "period").to_ascii_lowercase(), "utcOffsetMinutes": 480,
            "startsAt": start, "endsAt": end }),
        _ if period_start > 0 && period_end > period_start => json!({ "reset": "total", "startsAt": period_start.max(start), "endsAt": period_end }),
        _ => json!({ "reset": "total", "startsAt": start, "endsAt": end }),
    };
    row["localEstimate"] = json!(used.is_none() && remaining.is_none());
    row["unlimited"] = data.get("unlimited").cloned().unwrap_or(json!(false));
    if !output.iter().any(|existing| text(existing, "id") == id) { output.push(row); }
}

fn append_monitor(provider: &str, monitor: &Value, output: &mut Vec<Value>) {
    for limit in monitor.get("limits").and_then(Value::as_array).into_iter().flatten() {
        let kind = text(limit, "type");
        let unit = number(limit.get("unit")).unwrap_or(0.0) as i64;
        let period = number(limit.get("number")).unwrap_or(1.0);
        let label = match unit {
            3 => format!("{period} 小时窗口"), 4 => "每日窗口".to_string(),
            5 => "每月窗口".to_string(), 6 => "每周窗口".to_string(),
            _ => format!("额度窗口 {unit}"),
        };
        let total = number(limit.get("usage"));
        let used = number(limit.get("currentValue"));
        let remaining = number(limit.get("remaining")).or_else(|| Some(total? - used?));
        let mut row = source(provider, &format!("coding:{kind}:{unit}:{period}"), &format!("{kind} · {label}"), "", remaining, total, used);
        row["channel"] = json!("coding-plan");
        row["remainingPercent"] = number(limit.get("percentage")).map(|used| json!((100.0 - used).clamp(0.0, 100.0))).unwrap_or(Value::Null);
        row["resetsAt"] = json!(millis(limit.get("nextResetTime")));
        output.push(row);
    }
}
