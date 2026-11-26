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

fn window_rank(code: &str) -> u8 {
    match code.trim().to_ascii_lowercase().as_str() {
        "5h" => 0,
        "daily" => 1,
        "weekly" => 2,
        "monthly" => 3,
        _ => 4,
    }
}

const SHORT_WINDOW_MAX_MS: i64 = 6 * 3_600_000;

struct Entry {
    rank: u8,
    order: usize,
    row: Value,
    wallet: Value,
    value: f64,
    name: String,
}

struct GroupEntries {
    group_name: String,
    has_5h: bool,
    exhausted_weekly_models: Option<Value>,
    entries: Vec<Entry>,
}

fn set_entry_percent(entry: &mut Entry, percent: f64) {
    let reading = format!("剩 {}%", percent_text(percent));
    entry.value = percent;
    entry.row["remainingPercent"] = json!(percent);
    entry.row["view"] = json!(reading);
    entry.wallet["remainingPercent"] = json!(percent);
    entry.wallet["balanceView"] = json!(reading);
}

fn set_entry_reset(entry: &mut Entry, reset_at: Option<i64>) {
    match reset_at {
        Some(at) => {
            entry.row["resetsAt"] = json!(at);
            entry.wallet["resetsAt"] = json!(at);
        }
        None => {
            if let Some(obj) = entry.row.as_object_mut() {
                obj.remove("resetsAt");
            }
            if let Some(obj) = entry.wallet.as_object_mut() {
                obj.remove("resetsAt");
            }
        }
    }
}

fn pick_model_reset(resets: &[i64], now_ms: i64) -> Option<i64> {
    if now_ms > 0 {
        if let Some(future) = resets.iter().copied().filter(|at| *at > now_ms).min() {
            return Some(future);
        }
    }
    resets.iter().copied().min()
}

/// 当上游分组汇总漏报（例如仍返回 100%），而该组下所有逐模型读数一致更低/归零时，
/// 用真实模型配额上限校准分组窗口，并按恢复时长区分短窗直改与跨天/周级长冷却。
fn reconcile_group_with_models(
    group: &mut GroupEntries,
    model_quotas: &[Value],
    now_ms: i64,
) {
    if group.group_name.is_empty() || group.entries.is_empty() || model_quotas.is_empty() {
        return;
    }
    let matched: Vec<(f64, Option<i64>)> = model_quotas
        .iter()
        .filter_map(|item| {
            let item_group = item["group"].as_str().map(str::trim).unwrap_or("");
            if !item_group.eq_ignore_ascii_case(&group.group_name) {
                return None;
            }
            let pct = item["modelPercent"]
                .as_f64()
                .filter(|v| v.is_finite())?
                .clamp(0.0, 100.0);
            let reset = item["modelResetsAt"]
                .as_i64()
                .or_else(|| item["resetsAt"].as_i64());
            Some((pct, reset))
        })
        .collect();
    if matched.is_empty() {
        return;
    }

    let max_model_pct = matched
        .iter()
        .map(|(pct, _)| *pct)
        .fold(f64::NEG_INFINITY, f64::max);
    let min_entry_value = group
        .entries
        .iter()
        .map(|e| e.value)
        .fold(f64::INFINITY, f64::min);
    if max_model_pct + 1e-6 >= min_entry_value {
        return;
    }

    let resets: Vec<i64> = matched
        .iter()
        .filter(|(pct, _)| *pct < 100.0)
        .filter_map(|(_, reset)| *reset)
        .collect();
    let model_reset = pick_model_reset(&resets, now_ms);
    let is_long_reset =
        model_reset.is_some_and(|at| now_ms > 0 && at - now_ms > SHORT_WINDOW_MAX_MS);

    let idx_5h = group
        .entries
        .iter()
        .position(|e| e.row["window"].as_str() == Some("5h"));

    match idx_5h {
        Some(idx) if !is_long_reset => {
            set_entry_percent(&mut group.entries[idx], max_model_pct);
            if max_model_pct < 100.0 {
                if let Some(at) = model_reset {
                    set_entry_reset(&mut group.entries[idx], Some(at));
                }
            }
        }
        Some(idx) => {
            {
                let entry_5h = &mut group.entries[idx];
                set_entry_percent(entry_5h, max_model_pct);
                set_entry_reset(entry_5h, None);
                entry_5h.row["constrainedByModel"] = json!(true);
                if let Some(at) = model_reset {
                    entry_5h.row["modelResetsAt"] = json!(at);
                }
            }
            for entry in &mut group.entries {
                if entry.row["window"].as_str() == Some("weekly")
                    && entry.value > max_model_pct + 1e-6
                {
                    entry.row["constrainedByModel"] = json!(true);
                    if let Some(at) = model_reset {
                        entry.row["modelResetsAt"] = json!(at);
                    }
                }
            }
        }
        None => {
            let target_idx = group
                .entries
                .iter()
                .position(|e| e.row["window"].as_str() == Some("weekly"))
                .unwrap_or(0);
            set_entry_percent(&mut group.entries[target_idx], max_model_pct);
            if max_model_pct < 100.0 {
                if let Some(at) = model_reset {
                    set_entry_reset(&mut group.entries[target_idx], Some(at));
                }
            }
        }
    }
}

#[cfg(test)]
pub fn wallets(
    payload: &Value,
    models: &[Value],
    paid_tier: bool,
) -> (Vec<Value>, Vec<Value>, String) {
    wallets_with_models(payload, models, &[], paid_tier, 0)
}

pub fn wallets_with_models(
    payload: &Value,
    models: &[Value],
    model_quotas: &[Value],
    paid_tier: bool,
    now_ms: i64,
) -> (Vec<Value>, Vec<Value>, String) {

    let mut parsed_groups: Vec<GroupEntries> = Vec::new();
    for group in payload["groups"].as_array().into_iter().flatten() {
        let group_name = group_name(&util::text(group, &["displayName", "display_name"]));
        let mut has_5h = false;
        let mut exhausted_weekly_models = None;
        let mut entries = Vec::new();

        for (order, bucket) in group["buckets"].as_array().into_iter().flatten().enumerate() {
            let Some(fraction) = util::bucket_fraction(bucket) else {
                continue;
            };
            let value = fraction * 100.0;
            if !value.is_finite() {
                continue;
            }
            let code = window_code(bucket);
            let window = window_name(bucket);
            let name = if group_name.is_empty() {
                window.clone()
            } else {
                format!("{group_name} · {window}")
            };
            let raw_key = util::text(bucket, &["bucketId", "bucket_id"]);
            let reset = (fraction < 1.0)
                .then(|| util::rfc3339(&util::text(bucket, &["resetTime", "reset_time"])))
                .flatten();
            let reading = format!("剩 {}%", percent_text(value));
            let key = if raw_key.is_empty() {
                format!("{group_name}:{code}")
            } else {
                raw_key
            };
            let bucket_models = argon::quota_models(models, payload, group, bucket);
            let mut row = json!({
                "key": key,
                "label": name,
                "group": group_name,
                "window": code,
                "remainingPercent": value,
                "view": reading,
                "models": bucket_models
            });
            if let Some(at) = reset {
                row["resetsAt"] = json!(at);
            }
            let mut wallet = json!({
                "type": "quota_window",
                "displayName": name,
                "balance": null,
                "balanceView": reading,
                "remainingPercent": value
            });
            if let Some(at) = reset {
                wallet["resetsAt"] = json!(at);
            }
            if code == "5h" {
                has_5h = true;
            } else if code == "weekly" && value <= 0.0 && exhausted_weekly_models.is_none() {
                exhausted_weekly_models = Some(row["models"].clone());
            }
            entries.push(Entry {
                rank: window_rank(&code),
                order,
                row,
                wallet,
                value,
                name,
            });
        }
        if !entries.is_empty() {
            parsed_groups.push(GroupEntries {
                group_name,
                has_5h,
                exhausted_weekly_models,
                entries,
            });
        }
    }

    // 付费/双窗账号当每周耗尽（0%）时，上游可能直接省掉 5 小时桶；仅对双窗常规模型组补回 0% 短窗。
    let recover_dual_5h = paid_tier || parsed_groups.iter().any(|group| group.has_5h);
    if recover_dual_5h {
        for group in &mut parsed_groups {
            if group.has_5h || !matches!(group.group_name.as_str(), "Gemini" | "Claude/GPT") {
                continue;
            }
            let Some(weekly_models) = group.exhausted_weekly_models.take() else {
                continue;
            };
            let name = format!("{} · 5 小时", group.group_name);
            let reading = "剩 0%".to_string();
            let key = format!("{}:5h", group.group_name);
            group.entries.push(Entry {
                rank: window_rank("5h"),
                order: usize::MAX,
                row: json!({
                    "key": key,
                    "label": name,
                    "group": group.group_name,
                    "window": "5h",
                    "remainingPercent": 0.0,
                    "view": reading,
                    "models": weekly_models
                }),
                wallet: json!({
                    "type": "quota_window",
                    "displayName": name,
                    "balance": null,
                    "balanceView": reading,
                    "remainingPercent": 0.0
                }),
                value: 0.0,
                name,
            });
        }
    }

    let mut wallets = Vec::new();
    let mut windows = Vec::new();
    let mut tightest: Option<(f64, String)> = None;
    for mut group in parsed_groups {
        reconcile_group_with_models(&mut group, model_quotas, now_ms);
        group
            .entries
            .sort_by_key(|entry| (entry.rank, entry.order));
        for entry in group.entries {
            windows.push(entry.row);
            wallets.push(entry.wallet);
            if tightest.as_ref().is_none_or(|(old, _)| entry.value < *old) {
                tightest = Some((entry.value, entry.name));
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
        if let Some(at) = row["resetsAt"].as_i64().or_else(|| {
            (row["window"].as_str() == Some("5h"))
                .then(|| row["modelResetsAt"].as_i64())
                .flatten()
        }) {
            source["resetsAt"] = json!(at);
        }
        source
    }).collect();
    json!(sources)
}

#[cfg(test)]
mod tests {
    use super::wallets;
    use agent2api_wasm_sdk::{json, Value};

    fn window_pairs(windows: &[Value]) -> Vec<(&str, &str, f64)> {
        windows
            .iter()
            .map(|row| {
                (
                    row["group"].as_str().unwrap_or(""),
                    row["window"].as_str().unwrap_or(""),
                    row["remainingPercent"].as_f64().unwrap_or(-1.0),
                )
            })
            .collect()
    }

    #[test]
    fn wallets_orders_5h_before_weekly_while_preserving_group_order() {
        let summary = json!({
            "groups": [
                {
                    "displayName": "Gemini",
                    "buckets": [
                        { "bucketId": "g-weekly", "remainingFraction": 0.6025, "resetTime": "2026-11-30T00:00:00Z" },
                        { "bucketId": "g-5h", "remainingFraction": 0.1313, "resetTime": "2026-11-23T18:00:00Z" }
                    ]
                },
                {
                    "displayName": "Claude/GPT Models",
                    "buckets": [
                        { "bucketId": "3p-weekly", "remainingFraction": 1.0 },
                        { "bucketId": "3p-5h", "remainingFraction": 1.0 }
                    ]
                },
                {
                    "displayName": "Gemini 4 Argon",
                    "buckets": [
                        { "bucketId": "argon-weekly", "remainingFraction": 0.0106, "resetTime": "2026-11-30T00:00:00Z" }
                    ]
                }
            ]
        });

        let (wallet_rows, windows, _) = wallets(&summary, &[], true);
        assert_eq!(
            window_pairs(&windows),
            vec![
                ("Gemini", "5h", 13.13),
                ("Gemini", "weekly", 60.25),
                ("Claude/GPT", "5h", 100.0),
                ("Claude/GPT", "weekly", 100.0),
                ("Gemini 4 Argon", "weekly", 1.06),
            ]
        );
        let wallet_names: Vec<&str> = wallet_rows
            .iter()
            .filter_map(|row| row["displayName"].as_str())
            .collect();
        assert_eq!(
            wallet_names,
            vec![
                "Gemini · 5 小时",
                "Gemini · 每周",
                "Claude/GPT · 5 小时",
                "Claude/GPT · 每周",
                "Gemini 4 Argon · 每周",
            ]
        );
    }

    #[test]
    fn wallets_keeps_disabled_5h_bucket_when_weekly_exhausted() {
        let summary = json!({
            "groups": [
                {
                    "displayName": "Gemini",
                    "buckets": [
                        { "bucketId": "g-weekly", "remainingFraction": 0.432, "resetTime": "2026-11-30T00:00:00Z" },
                        { "bucketId": "g-5h", "remainingFraction": 1.0 }
                    ]
                },
                {
                    "displayName": "Claude/GPT Models",
                    "buckets": [
                        { "bucketId": "3p-weekly", "remainingFraction": 0.0, "resetTime": "2026-11-30T00:00:00Z" },
                        { "bucketId": "3p-5h", "disabled": true, "remainingFraction": 1.0 }
                    ]
                }
            ]
        });

        let (_, windows, available) = wallets(&summary, &[], false);
        assert_eq!(
            window_pairs(&windows),
            vec![
                ("Gemini", "5h", 100.0),
                ("Gemini", "weekly", 43.2),
                ("Claude/GPT", "5h", 0.0),
                ("Claude/GPT", "weekly", 0.0),
            ]
        );
        assert_eq!(available, "Claude/GPT · 5 小时 剩 0%");
    }

    #[test]
    fn wallets_keeps_omitted_fraction_5h_bucket_as_zero() {
        let summary = json!({
            "groups": [
                {
                    "displayName": "Claude/GPT Models",
                    "buckets": [
                        { "bucketId": "3p-weekly", "resetTime": "2026-11-30T00:00:00Z" },
                        { "bucketId": "3p-5h" }
                    ]
                }
            ]
        });

        let (_, windows, _) = wallets(&summary, &[], false);
        assert_eq!(
            window_pairs(&windows),
            vec![("Claude/GPT", "5h", 0.0), ("Claude/GPT", "weekly", 0.0)]
        );
    }

    #[test]
    fn wallets_recovers_missing_5h_for_exhausted_dual_window_groups_only() {
        let summary = json!({
            "groups": [
                {
                    "displayName": "Gemini",
                    "buckets": [
                        { "bucketId": "g-weekly", "remainingFraction": 0.432, "resetTime": "2026-11-30T00:00:00Z" },
                        { "bucketId": "g-5h", "remainingFraction": 1.0 }
                    ]
                },
                {
                    "displayName": "Claude/GPT Models",
                    "buckets": [
                        { "bucketId": "3p-weekly", "remainingFraction": 0.0, "resetTime": "2026-11-30T00:00:00Z" }
                    ]
                },
                {
                    "displayName": "Gemini 4 Argon",
                    "buckets": [
                        { "bucketId": "argon-weekly", "remainingFraction": 0.0, "resetTime": "2026-11-30T00:00:00Z" }
                    ]
                }
            ]
        });

        let (_, windows, _) = wallets(&summary, &[], true);
        assert_eq!(
            window_pairs(&windows),
            vec![
                ("Gemini", "5h", 100.0),
                ("Gemini", "weekly", 43.2),
                ("Claude/GPT", "5h", 0.0),
                ("Claude/GPT", "weekly", 0.0),
                ("Gemini 4 Argon", "weekly", 0.0),
            ]
        );
        assert!(windows[2]["resetsAt"].is_null());

        let all_exhausted_paid = json!({
            "groups": [
                {
                    "displayName": "Gemini",
                    "buckets": [{ "bucketId": "g-weekly", "remainingFraction": 0.0 }]
                },
                {
                    "displayName": "Claude/GPT Models",
                    "buckets": [{ "bucketId": "3p-weekly", "remainingFraction": 0.0 }]
                }
            ]
        });
        let (_, paid_windows, _) = wallets(&all_exhausted_paid, &[], true);
        assert_eq!(
            window_pairs(&paid_windows),
            vec![
                ("Gemini", "5h", 0.0),
                ("Gemini", "weekly", 0.0),
                ("Claude/GPT", "5h", 0.0),
                ("Claude/GPT", "weekly", 0.0),
            ]
        );
    }

    #[test]
    fn wallets_keeps_free_tier_weekly_only_when_no_5h_exists() {
        let summary = json!({
            "groups": [
                {
                    "displayName": "Gemini",
                    "buckets": [{ "bucketId": "g-weekly", "remainingFraction": 0.0 }]
                },
                {
                    "displayName": "Claude/GPT Models",
                    "buckets": [{ "bucketId": "3p-weekly", "remainingFraction": 0.0 }]
                }
            ]
        });

        let (_, free_windows, _) = wallets(&summary, &[], false);
        assert_eq!(
            window_pairs(&free_windows),
            vec![("Gemini", "weekly", 0.0), ("Claude/GPT", "weekly", 0.0)]
        );
    }

    #[test]
    fn wallets_reconciles_long_group_wide_model_zero_without_faking_weekly() {
        let now_ms = 1_795_700_000_000_i64;
        let reset_at = now_ms + 23 * 3_600_000 + 17 * 60_000;
        let summary = json!({
            "groups": [
                {
                    "displayName": "Gemini Models",
                    "buckets": [
                        { "bucketId": "gemini-5h", "remainingFraction": 1.0 },
                        { "bucketId": "gemini-weekly", "remainingFraction": 1.0 }
                    ]
                },
                {
                    "displayName": "Claude and GPT models",
                    "buckets": [
                        { "bucketId": "3p-weekly", "remainingFraction": 1.0 },
                        { "bucketId": "3p-5h", "remainingFraction": 1.0 }
                    ]
                }
            ]
        });
        let model_quotas = vec![
            json!({ "id": "gemini-4-neon", "group": "Gemini", "modelPercent": 100.0 }),
            json!({ "id": "claude-opus-5-5", "group": "Claude/GPT", "modelPercent": 0.0, "modelResetsAt": reset_at }),
            json!({ "id": "claude-sonnet-5-5", "group": "Claude/GPT", "modelPercent": 0.0, "modelResetsAt": reset_at }),
            json!({ "id": "gpt-oss-120b", "group": "Claude/GPT", "modelPercent": 0.0, "modelResetsAt": reset_at }),
        ];

        let (_, windows, available) = super::wallets_with_models(&summary, &[], &model_quotas, true, now_ms);
        assert_eq!(
            window_pairs(&windows),
            vec![
                ("Gemini", "5h", 100.0),
                ("Gemini", "weekly", 100.0),
                ("Claude/GPT", "5h", 0.0),
                ("Claude/GPT", "weekly", 100.0),
            ]
        );
        assert_eq!(available, "Claude/GPT · 5 小时 剩 0%");
        assert_eq!(windows[2]["constrainedByModel"], json!(true));
        assert_eq!(windows[2]["modelResetsAt"], json!(reset_at));
        assert!(windows[2]["resetsAt"].is_null());
        assert_eq!(windows[3]["constrainedByModel"], json!(true));
        assert_eq!(windows[3]["modelResetsAt"], json!(reset_at));

        let sources = super::sources(&json!({ "windows": windows }));
        let source_5h = sources.as_array().unwrap().iter().find(|s| s["id"] == "window:3p-5h").unwrap();
        let source_weekly = sources.as_array().unwrap().iter().find(|s| s["id"] == "window:3p-weekly").unwrap();
        assert_eq!(source_5h["remainingPercent"], json!(0.0));
        assert_eq!(source_5h["resetsAt"], json!(reset_at));
        assert_eq!(source_weekly["remainingPercent"], json!(100.0));
        assert!(source_weekly["resetsAt"].is_null());
    }

    #[test]
    fn wallets_reconciles_short_group_wide_model_zero_onto_5h_only() {
        let now_ms = 1_795_700_000_000_i64;
        let reset_at = now_ms + 3 * 3_600_000;
        let summary = json!({
            "groups": [
                {
                    "displayName": "Claude and GPT models",
                    "buckets": [
                        { "bucketId": "3p-weekly", "remainingFraction": 1.0 },
                        { "bucketId": "3p-5h", "remainingFraction": 1.0 }
                    ]
                }
            ]
        });
        let model_quotas = vec![
            json!({ "id": "claude-opus-5-5", "group": "Claude/GPT", "modelPercent": 0.0, "modelResetsAt": reset_at }),
            json!({ "id": "claude-sonnet-5-5", "group": "Claude/GPT", "modelPercent": 0.0, "modelResetsAt": reset_at }),
        ];

        let (_, windows, _) = super::wallets_with_models(&summary, &[], &model_quotas, true, now_ms);
        assert_eq!(
            window_pairs(&windows),
            vec![("Claude/GPT", "5h", 0.0), ("Claude/GPT", "weekly", 100.0)]
        );
        assert_eq!(windows[0]["resetsAt"], json!(reset_at));
        assert!(windows[0].get("constrainedByModel").is_none());
        assert!(windows[1].get("constrainedByModel").is_none());
    }

    #[test]
    fn wallets_does_not_reconcile_when_only_one_model_in_group_is_zero() {
        let now_ms = 1_795_700_000_000_i64;
        let reset_at = now_ms + 20 * 3_600_000;
        let summary = json!({
            "groups": [
                {
                    "displayName": "Gemini Models",
                    "buckets": [
                        { "bucketId": "gemini-5h", "remainingFraction": 1.0 },
                        { "bucketId": "gemini-weekly", "remainingFraction": 1.0 }
                    ]
                }
            ]
        });
        let model_quotas = vec![
            json!({ "id": "gemini-3.1-flash-image", "group": "Gemini", "modelPercent": 0.0, "modelResetsAt": reset_at }),
            json!({ "id": "gemini-4-neon", "group": "Gemini", "modelPercent": 100.0 }),
        ];

        let (_, windows, _) = super::wallets_with_models(&summary, &[], &model_quotas, true, now_ms);
        assert_eq!(
            window_pairs(&windows),
            vec![("Gemini", "5h", 100.0), ("Gemini", "weekly", 100.0)]
        );
    }

    #[test]
    fn wallets_keeps_tighter_weekly_when_summary_already_reports_bottleneck() {
        let now_ms = 1_795_700_000_000_i64;
        let summary = json!({
            "groups": [
                {
                    "displayName": "Gemini Models",
                    "buckets": [
                        { "bucketId": "gemini-weekly", "remainingFraction": 0.8747867, "resetTime": "2026-12-03T07:56:02Z" },
                        { "bucketId": "gemini-5h", "remainingFraction": 0.887309, "resetTime": "2026-11-26T21:26:48Z" }
                    ]
                }
            ]
        });
        let model_quotas = vec![
            json!({ "id": "gemini-4-neon", "group": "Gemini", "modelPercent": 88.73 }),
        ];

        let (_, windows, _) = super::wallets_with_models(&summary, &[], &model_quotas, true, now_ms);
        assert_eq!(
            window_pairs(&windows),
            vec![("Gemini", "5h", 88.7309), ("Gemini", "weekly", 0.8747867 * 100.0)]
        );
        assert!(windows[0].get("constrainedByModel").is_none());
        assert!(windows[1].get("constrainedByModel").is_none());
    }

    #[test]
    fn wallets_preserves_partial_weekly_usage_when_long_model_limit_hits_zero() {
        let now_ms = 1_795_700_000_000_i64;
        let reset_at = now_ms + 23 * 3_600_000;
        let weekly_reset_str = "2026-12-03T07:56:02Z";
        let summary = json!({
            "groups": [
                {
                    "displayName": "Claude and GPT models",
                    "buckets": [
                        { "bucketId": "3p-weekly", "remainingFraction": 0.62, "resetTime": weekly_reset_str },
                        { "bucketId": "3p-5h", "remainingFraction": 1.0 }
                    ]
                }
            ]
        });
        let model_quotas = vec![
            json!({ "id": "claude-opus-5-5", "group": "Claude/GPT", "modelPercent": 0.0, "modelResetsAt": reset_at }),
        ];

        let (_, windows, _) = super::wallets_with_models(&summary, &[], &model_quotas, true, now_ms);
        assert_eq!(
            window_pairs(&windows),
            vec![("Claude/GPT", "5h", 0.0), ("Claude/GPT", "weekly", 62.0)]
        );
        assert_eq!(windows[1]["constrainedByModel"], json!(true));
        assert_eq!(windows[1]["modelResetsAt"], json!(reset_at));
        assert!(windows[1]["resetsAt"].is_i64());
    }
}
