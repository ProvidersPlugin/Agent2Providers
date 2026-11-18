//! 原生通用 6 档与 Accio 模型声明档位收敛，不以模型名猜档位放置位置。

use agent2api_wasm_sdk::{json, Value};

const LEVELS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

pub fn off(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case("off") || value.trim().eq_ignore_ascii_case("none")
}
fn rank(value: &str) -> Option<usize> {
    LEVELS
        .iter()
        .position(|level| level.eq_ignore_ascii_case(value.trim()))
}
fn normalized(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.chars().count() <= 32 && !value.chars().any(char::is_control))
        .then(|| value.to_string())
}

pub fn client(body: &Value) -> Option<String> {
    for key in [
        "reasoning_effort",
        "reasoningEffort",
        "effort",
        "reasoning",
        "thinking",
    ] {
        match body.get(key) {
            Some(Value::String(text)) => return normalized(text),
            Some(Value::Object(object)) if key == "reasoning" => {
                if let Some(level) = object
                    .get("effort")
                    .and_then(Value::as_str)
                    .and_then(normalized)
                {
                    return Some(level);
                }
            }
            _ => {}
        }
    }
    None
}

pub fn resolve(body: &Value, entry: &Value) -> Option<String> {
    let efforts: Vec<&str> = entry["reasoningEfforts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let level = client(body)?;
    if off(&level) {
        return None;
    }
    let wanted = match rank(&level) {
        Some(rank) => rank,
        None => {
            return efforts
                .iter()
                .find(|effort| effort.eq_ignore_ascii_case(&level))
                .map(|effort| (*effort).to_string())
        }
    };
    efforts
        .iter()
        .filter_map(|effort| rank(effort).map(|rank| (rank.abs_diff(wanted), *effort)))
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, effort)| effort)
        .or_else(|| efforts.first().copied())
        .map(str::to_string)
}

pub fn patch(level: &str, body: &Value) -> Value {
    let reason = if off(level) {
        Some("Accio 没有关闭思考的可靠表达，跳过注入")
    } else if client(body).is_some() {
        Some("客户端已指定思考等级，绑定让位")
    } else if rank(level).is_none() {
        Some("该等级不在通用档位表内，不注入未知档位")
    } else {
        None
    };
    match reason {
        Some(reason) => json!({ "action": "skip", "reason": reason }),
        None => json!({ "action": "set", "field": "reasoning_effort", "value": level }),
    }
}
