//! 原生 balance.rs 的百分比钱包/窗口展示；缺读数不写零、重置时刻只提供RFC3339毫秒。

use super::{argon, util};
use agent2api_wasm_sdk::{json, Value};

pub fn normalize_tier(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("ultra") || lower.contains("helium") {
        "Ultra".into()
    } else if lower.contains("free") || lower.contains("starter") {
        "Free".into()
    } else if lower.contains("pro") || lower.contains("premium") || lower.contains("advanced") {
        "Pro".into()
    } else {
        raw.trim().into()
    }
}
pub fn group_name(raw: &str) -> String {
    let mut name = raw.trim();
    let lower = name.to_ascii_lowercase();
    for suffix in [" models", " model"] {
        if lower.ends_with(suffix) {
            name = name
                .get(..name.len() - suffix.len())
                .unwrap_or(name)
                .trim_end();
            break;
        }
    }
    let lower = name.to_ascii_lowercase();
    if lower.contains("claude") && lower.contains("gpt") {
        "Claude/GPT".into()
    } else {
        name.into()
    }
}
fn raw_window(bucket: &Value) -> String {
    util::text(bucket, &["window", "bucketId", "bucket_id"])
}
pub fn window_code(bucket: &Value) -> String {
    let raw = raw_window(bucket);
    let lower = raw.to_ascii_lowercase();
    if lower.contains("week") || lower.contains("7d") {
        "weekly".into()
    } else if lower.contains("5h") || lower.contains("hour") {
        "5h".into()
    } else if lower.contains("month") {
        "monthly".into()
    } else if lower.contains("day") || lower.contains("24h") {
        "daily".into()
    } else {
        raw
    }
}
fn window_name(bucket: &Value) -> String {
    let name = util::text(bucket, &["displayName", "display_name"]);
    if !name.is_empty() {
        return name;
    }
    let raw = raw_window(bucket);
    match window_code(bucket).as_str() {
        "weekly" => "每周".into(),
        "5h" => "5 小时".into(),
        _ if raw.is_empty() => "额度".into(),
        _ => raw,
    }
}
pub fn percent_text(value: f64) -> String {
    format!("{value:.1}").trim_end_matches(".0").to_string()
}

pub fn wallets(payload: &Value, models: &[Value]) -> (Vec<Value>, Vec<Value>, String) {
    let mut wallets = Vec::new();
    let mut windows = Vec::new();
    let mut tightest: Option<(f64, String)> = None;
    for group in payload["groups"].as_array().into_iter().flatten() {
        let group_name = group_name(&util::text(group, &["displayName", "display_name"]));
        for bucket in group["buckets"].as_array().into_iter().flatten() {
            if bucket["disabled"].as_bool() == Some(true) {
                continue;
            }
            let Some(fraction) = util::quota_fraction(bucket) else {
                continue;
            };
            let value = fraction * 100.0;
            if !value.is_finite() {
                continue;
            }
            let window = window_name(bucket);
            let name = if group_name.is_empty() {
                window.clone()
            } else {
                format!("{group_name} · {window}")
            };
            let key = util::text(bucket, &["bucketId", "bucket_id"]);
            let reset = (fraction < 1.0)
                .then(|| util::rfc3339(&util::text(bucket, &["resetTime", "reset_time"])))
                .flatten();
            let reading = format!("剩 {}%", percent_text(value));
            let key = if key.is_empty() {
                format!("{}:{}", group_name, window_code(bucket))
            } else {
                key
            };
            let mut row = json!({ "key": key, "label": name,
                "group": group_name, "window": window_code(bucket), "remainingPercent": value,
                "view": reading, "models": argon::quota_models(models, payload, group, bucket) });
            if let Some(at) = reset {
                row["resetsAt"] = json!(at);
            }
            windows.push(row);
            let mut wallet = json!({ "type": "quota_window", "displayName": name,
                "balance": null, "balanceView": reading, "remainingPercent": value });
            if let Some(at) = reset {
                wallet["resetsAt"] = json!(at);
            }
            wallets.push(wallet);
            if tightest.as_ref().is_none_or(|(old, _)| value < *old) {
                tightest = Some((value, name));
            }
        }
    }
    let view = tightest
        .map(|(value, name)| format!("{name} 剩 {}%", percent_text(value)))
        .unwrap_or_else(|| "无额度窗口".into());
    (wallets, windows, view)
}

/// 当分组汇总接口暂不可用时，按逐模型读数聚合成兜底分组窗口，避免面板与限额器空白。
pub fn fallback_from_models(model_quotas: &[Value]) -> (Vec<Value>, Vec<Value>, String) {
    struct Acc {
        group: String,
        window: String,
        min_pct: f64,
        reset: Option<i64>,
        models: Vec<String>,
    }
    let mut grouped: Vec<Acc> = Vec::new();
    for item in model_quotas {
        let Some(pct) = item["remainingPercent"].as_f64().filter(|v| v.is_finite()) else {
            continue;
        };
        let group = item["group"].as_str().unwrap_or("Gemini").trim();
        let group = if group.is_empty() { "Gemini" } else { group };
        let id = item["id"].as_str().unwrap_or("");
        let reset = item["resetsAt"].as_i64();
        let win = item["window"].as_str().unwrap_or("");
        if let Some(acc) = grouped.iter_mut().find(|g| g.group == group) {
            if pct < acc.min_pct {
                acc.min_pct = pct;
                acc.reset = reset.or(acc.reset);
                if !win.is_empty() {
                    acc.window = win.to_string();
                }
            }
            if !id.is_empty() && !acc.models.iter().any(|known| known == id) {
                acc.models.push(id.to_string());
            }
        } else {
            grouped.push(Acc {
                group: group.to_string(),
                window: if win.is_empty() { "额度".into() } else { win.to_string() },
                min_pct: pct,
                reset,
                models: if id.is_empty() { Vec::new() } else { vec![id.to_string()] },
            });
        }
    }
    let mut wallets = Vec::new();
    let mut windows = Vec::new();
    let mut tightest: Option<(f64, String)> = None;
    for acc in grouped {
        let win_label = match acc.window.as_str() {
            "weekly" => "每周",
            "5h" => "5 小时",
            other => other,
        };
        let label = format!("{} · {}", acc.group, win_label);
        let reading = format!("剩 {}%", percent_text(acc.min_pct));
        let key = format!("model-fallback:{}", acc.group.to_ascii_lowercase());
        let mut row = json!({
            "key": key,
            "label": label,
            "group": acc.group,
            "window": acc.window,
            "remainingPercent": acc.min_pct,
            "view": reading,
            "models": acc.models,
        });
        if let Some(at) = acc.reset {
            row["resetsAt"] = json!(at);
        }
        windows.push(row);
        let mut wallet = json!({
            "type": "quota_window",
            "displayName": label,
            "balance": null,
            "balanceView": reading,
            "remainingPercent": acc.min_pct,
        });
        if let Some(at) = acc.reset {
            wallet["resetsAt"] = json!(at);
        }
        wallets.push(wallet);
        if tightest.as_ref().is_none_or(|(old, _)| acc.min_pct < *old) {
            tightest = Some((acc.min_pct, label));
        }
    }
    let view = tightest
        .map(|(value, name)| format!("{name} 剩 {}%", percent_text(value)))
        .unwrap_or_else(|| "无额度窗口".into());
    (wallets, windows, view)
}

pub fn sources(report: &Value) -> Value {
    let sources: Vec<Value> = report["windows"].as_array().into_iter().flatten().map(|row| {
        let mut source = json!({ "id": format!("window:{}", row["key"].as_str().unwrap_or("")),
            "label": row["label"], "provider": super::PROVIDER_ID, "unit": "%",
            "remaining": null, "total": null, "used": null, "remainingPercent": row["remainingPercent"],
            "models": row["models"] });
        if let Some(at) = row["resetsAt"].as_i64() { source["resetsAt"] = json!(at); }
        source
    }).collect();
    json!(sources)
}
