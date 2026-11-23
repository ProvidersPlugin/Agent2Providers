//! 只投影真实的额度窗口。缺值用 null，窗口互相包含时不求和，也不换算 token。
//! 时间字段按已核验的毫秒口径传递；没有日期就不设置重置或到期时间。

use agent2api_wasm_sdk::{json, Result, Value};

use crate::{errors, util};

pub struct Limit<'a> {
    pub group: &'a Value,
    pub value: &'a Value,
    pub kind: u64,
}

pub fn limits(section: &Value) -> Result<Vec<Limit<'_>>> {
    if section.is_null() {
        return Ok(Vec::new());
    }
    if !section.is_object() {
        return Err(errors::protocol("豆包工作额度分区格式无效"));
    }
    if section
        .get("usage_exhausted")
        .is_some_and(|value| !value.is_null() && !value.is_boolean())
    {
        return Err(errors::protocol("豆包工作额度耗尽标记格式无效"));
    }
    let groups = match section.get("window_limit_groups") {
        Some(Value::Array(groups)) if groups.len() <= 256 => groups,
        None | Some(Value::Null) => return Ok(Vec::new()),
        _ => return Err(errors::protocol("豆包工作额度分区的窗口列表格式无效")),
    };
    let mut output = Vec::new();
    for group in groups {
        if !group.is_object() {
            return Err(errors::protocol("豆包工作额度组不是对象"));
        }
        let rows = match group.get("window_limits") {
            Some(Value::Array(rows)) if rows.len() <= 256 => rows,
            None | Some(Value::Null) => continue,
            _ => return Err(errors::protocol("豆包工作额度组的窗口列表格式无效")),
        };
        for value in rows {
            if !value.is_object() {
                return Err(errors::protocol("豆包工作额度窗口不是对象"));
            }
            let kind = util::unsigned(&value["window_type"])
                .ok_or_else(|| errors::protocol("豆包工作额度窗口缺少类型编号"))?;
            output.push(Limit { group, value, kind });
            if output.len() > 512 {
                return Err(errors::protocol("豆包工作额度窗口过多"));
            }
        }
    }
    Ok(output)
}

pub fn usable(section: &Value, limits: &[Limit<'_>]) -> bool {
    section["usage_exhausted"] != true && limits.iter().any(|limit| matches!(limit.kind, 1 | 2 | 4))
}

#[derive(Default)]
pub struct Windows {
    pub windows: Vec<Value>,
    pub credits: Vec<Value>,
    pub sources: Vec<Value>,
    pub missing_amount: bool,
    pub invalid_amount: bool,
    pub invalid_date: bool,
    pub unknown_window: bool,
    pub bonus: Option<f64>,
}

impl Windows {
    pub fn push(
        &mut self,
        limit: &Limit<'_>,
        index: usize,
        bonus: bool,
        secrets: &[String],
    ) -> Result<()> {
        let value = limit.value;
        let (code, label) = if bonus {
            ("bonus", "奖励额度")
        } else {
            match limit.kind {
                1 => ("5h", "5 小时"),
                2 => ("weekly", "近 7 天"),
                3 => ("subscription", "订阅额度"),
                4 => ("monthly", "每月"),
                _ => {
                    self.unknown_window = true;
                    return Ok(());
                }
            }
        };
        let group = util::safe_text(
            util::text(limit.group, &["feature_group_name"]),
            secrets,
            100,
        );
        let title = if group.is_empty() || bonus {
            label.to_string()
        } else {
            format!("{group} {label}")
        };
        let key = format!("{code}-{index}");
        let total = util::number(&value["total_amount"]);
        let used = util::number(&value["used_amount"]);
        let remaining = total.zip(used).map(|(total, used)| (total - used).max(0.0));
        let percent = util::number(&value["used_percent"])
            .map(|used| (100.0 - used).clamp(0.0, 100.0))
            .or_else(|| {
                total
                    .zip(used)
                    .filter(|(total, _)| *total > 0.0)
                    .map(|(total, used)| ((total - used) / total * 100.0).clamp(0.0, 100.0))
            });
        for field in ["total_amount", "used_amount", "used_percent"] {
            if value
                .get(field)
                .is_some_and(|raw| !raw.is_null() && util::number(raw).is_none())
            {
                self.invalid_amount = true;
            }
        }
        if value
            .get("unlimited")
            .is_some_and(|value| !value.is_null() && !value.is_boolean())
            || value
                .get("exemption")
                .is_some_and(|value| !value.is_null() && !value.is_object())
            || value["exemption"]
                .get("active")
                .is_some_and(|value| !value.is_null() && !value.is_boolean())
        {
            return Err(errors::protocol("豆包工作不限额或豁免状态格式无效"));
        }
        let exempt = code == "5h" && value["exemption"]["active"] == true;
        let unlimited = if exempt || value["unlimited"] == true {
            Some(true)
        } else {
            value["unlimited"].as_bool()
        };
        let date = if exempt {
            &value["exemption"]["end_time"]
        } else {
            &value["end_time"]
        };
        let at = util::timestamp_ms(date);
        if !date.is_null() && util::integer(date) != Some(0) && at.is_none() {
            self.invalid_date = true;
        }
        let (remaining, total, used, percent) = if unlimited == Some(true) {
            (None, None, None, None)
        } else {
            (remaining, total, used, percent)
        };
        self.missing_amount |= remaining.is_none() && percent.is_none() && unlimited != Some(true);
        if bonus && self.bonus.is_none() {
            self.bonus = remaining;
        }
        let view = if unlimited == Some(true) {
            "不限".into()
        } else if let Some(remaining) = remaining {
            total
                .map(|total| format!("剩余 {remaining} / {total} 额度"))
                .unwrap_or_else(|| format!("剩余 {remaining} 额度"))
        } else if let Some(percent) = percent {
            format!("剩余 {percent:.0}%")
        } else {
            "—".into()
        };
        let mut window = json!({ "key": key, "label": title, "group": group, "window": code,
            "remaining": remaining, "limit": total, "remainingPercent": percent,
            "unit": "额度", "unlimited": unlimited, "view": view });
        let mut credit = json!({ "id": key, "name": title, "remain": remaining, "total": total,
            "used": used, "remainingPercent": percent, "unit": "额度", "unlimited": unlimited });
        let mut source = json!({ "id": key, "label": title, "provider": "doubaowork", "remaining": remaining,
            "total": total, "used": used, "unit": "额度", "remainingPercent": percent, "unlimited": unlimited });
        if bonus {
            window["expiresAt"] = json!(at);
            credit["expireAt"] = json!(at);
            source["endsAt"] = json!(at);
        } else {
            window["resetsAt"] = json!(at);
            credit["resetAt"] = json!(at);
            credit["rolling"] = json!(true);
            source["resetsAt"] = json!(at);
        }
        self.windows.push(window);
        self.credits.push(credit);
        self.sources.push(source);
        Ok(())
    }
}

pub fn plan(value: &Value, secrets: &[String]) -> Option<String> {
    let data = value.get("data").unwrap_or(value);
    [
        data.get("membership_display_name"),
        data.pointer("/current_subscription/display/short_name"),
        data.pointer("/current_subscription/display/product_name"),
        data.pointer("/subscription/display/short_name"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .find(|raw| !raw.trim().is_empty())
    .map(|raw| util::safe_text(raw, secrets, 120))
}

pub fn sources(report: &Value) -> Value {
    let mut result = Vec::new();
    for row in report["quotaSources"]
        .as_array()
        .into_iter()
        .flatten()
        .take(512)
    {
        if row["provider"] != "doubaowork" || row["unit"] != "额度" {
            continue;
        }
        let id = util::text(row, &["id"]);
        if id.is_empty() || id.len() > 128 {
            continue;
        }
        result.push(json!({ "id": util::safe_text(id, &[], 128), "provider": "doubaowork",
            "label": util::safe_text(util::text(row, &["label"]), &[], 160), "unit": "额度",
            "remaining": util::number(&row["remaining"]), "total": util::number(&row["total"]),
            "used": util::number(&row["used"]), "remainingPercent": util::number(&row["remainingPercent"]),
            "unlimited": row["unlimited"].as_bool(), "resetsAt": util::timestamp_ms(&row["resetsAt"]),
            "endsAt": util::timestamp_ms(&row["endsAt"]) }));
    }
    json!(result)
}
