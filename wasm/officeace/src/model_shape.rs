//! 官方白名单只是展示而不是权限闸门；0命中退回完整目录，-zx 主名优先归并。
//! 没有内置模型名，不编造工具/视觉/思考位；客户端字段原样透传，缺能力位表示未知。

use super::util;
use agent2api_wasm_sdk::{json, Error, Result, Value};

const WHITELIST: &[&str] = &[
    "glm-5",
    "glm-5.1",
    "glm-5.2",
    "glm-5.3",
    "glm-5.3-flash",
    "deepseek-v4-flash-0731",
    "deepseek-v4.1-flash",
    "deepseek-v4.1-flash-1017",
    "kimi-k3",
    "kimi-k3.1-pro-highspeed",
    "openpangu-2.1-pro",
];

pub fn parse(payload: &Value) -> Result<Vec<Value>> {
    let rows = payload
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| payload.as_array())
        .filter(|rows| rows.len() <= 4096)
        .ok_or_else(|| Error::http(502, "OfficeAce 模型目录格式未知，已保留旧清单"))?;
    let mut models = Vec::with_capacity(rows.len());
    let mut seen = std::collections::HashSet::new();
    for row in rows {
        let id = row["id"]
            .as_str()
            .map(str::trim)
            .filter(|id| !id.is_empty() && id.len() <= 512 && !id.chars().any(char::is_control))
            .ok_or_else(|| Error::http(502, "OfficeAce 模型条目缺少有效名称，已保留旧清单"))?;
        if !seen.insert(id.to_ascii_lowercase()) {
            continue;
        }
        let mut item = json!({ "id": id, "name": id, "display_name": row["display_name"].as_str().unwrap_or(id),
            "description": "", "context_length": row["context_window"].as_i64().unwrap_or(0),
            "max_output_tokens": 0, "credit_display": "" });
        for key in ["owned_by", "developer"] {
            if let Some(value) = row[key].as_str() {
                item[key] = json!(value);
            }
        }
        models.push(item);
    }
    Ok(models)
}

pub fn whitelist(models: Vec<Value>) -> Vec<Value> {
    let mut kept: Vec<(String, bool, Value)> = Vec::new();
    for row in &models {
        let id = row["id"].as_str().unwrap_or("");
        let variant = id.to_ascii_lowercase().ends_with("-zx");
        let name = if variant { &id[..id.len() - 3] } else { id };
        let key = name.to_ascii_lowercase();
        if !WHITELIST.contains(&key.as_str()) {
            continue;
        }
        let mut row = row.clone();
        if variant {
            row["id"] = json!(name);
            row["name"] = json!(name);
        }
        match kept.iter().position(|(old, _, _)| old == &key) {
            Some(index) if kept[index].1 && !variant => kept[index] = (key, false, row),
            Some(_) => {}
            None => kept.push((key, variant, row)),
        }
    }
    if kept.is_empty() {
        models
    } else {
        kept.into_iter().map(|(_, _, row)| row).collect()
    }
}

pub fn rates(payload: &Value) -> Vec<(String, f64)> {
    payload["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let name = row["model_name"].as_str()?.trim().to_lowercase();
            let number = util::number(&row["cost_coefficient"])?;
            (!name.is_empty() && number >= 0.0).then_some((name, number))
        })
        .collect()
}

pub fn attach(models: &mut [Value], rates: &[(String, f64)]) {
    for model in models {
        let name = model["name"].as_str().unwrap_or("").to_lowercase();
        if let Some((_, rate)) = rates.iter().find(|(key, _)| key == &name) {
            let value = format!("{rate:.2}")
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_string();
            model["credits"] = json!(format!("x{value} credits"));
        }
    }
}
