//! 思考档位按本账号模型目录声明优先，常见别名其次。
//! 三项静态模型不声明思考；远程若给档位则优先按原值，不把 high 折错为 xhigh。
use super::util;
use agent2api_wasm_sdk::{json, Value};

pub struct Choice {
    pub enabled: Option<bool>,
    pub effort: Option<String>,
}
pub fn declared(body: &Value) -> Option<&Value> {
    body.get("reasoning_effort")
        .or_else(|| body.get("reasoning"))
        .or_else(|| body.get("thinking"))
}
pub fn resolve(body: &Value, model: &Value) -> Choice {
    let raw = declared(body).unwrap_or(&Value::Null);
    if !util::truthy(&model["reasoning"])
        || raw == &Value::Bool(false)
        || matches!(raw.as_str(), Some("off" | "none" | "disabled"))
    {
        return Choice {
            enabled: None,
            effort: None,
        };
    }
    if raw.is_null() || raw == &Value::Bool(true) || model["supportsReasoningEffort"] == false {
        return Choice {
            enabled: Some(true),
            effort: None,
        };
    }
    let asked = raw
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| raw.to_string())
        .trim()
        .to_lowercase();
    let declared = model["efforts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    let wanted = if declared.contains(&asked.as_str()) {
        asked.as_str()
    } else {
        match asked.as_str() {
            "minimal" | "min" => "low",
            "high" | "max" => "xhigh",
            other => other,
        }
    };
    let pool = if declared.is_empty() {
        vec!["low", "medium", "xhigh"]
    } else {
        declared.clone()
    };
    let effort = if pool.contains(&wanted) {
        wanted
    } else {
        declared
            .iter()
            .copied()
            .find(|value| *value == "medium")
            .or_else(|| declared.first().copied())
            .unwrap_or("medium")
    };
    Choice {
        enabled: Some(true),
        effort: Some(effort.into()),
    }
}
pub fn patch(params: &Value) -> Value {
    if declared(&params["body"]).is_some_and(|value| !value.is_null()) {
        return json!({ "action": "skip", "reason": "客户端已经指定思考档位，绑定不覆盖" });
    }
    let level = params["level"].as_str().unwrap_or("").trim();
    if !["minimal", "low", "medium", "high", "xhigh", "max"]
        .iter()
        .any(|known| known.eq_ignore_ascii_case(level))
    {
        return json!({ "action": "skip", "reason": "此档位不在通用候选内，模型目录未确认支持" });
    }
    json!({ "action": "set", "field": "reasoning_effort", "value": level })
}
