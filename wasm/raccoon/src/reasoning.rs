//! 1.1.5 Box-Agent 思考字段规则，显式设置优先；仅已证实的模型家族作转换。

use agent2api_wasm_sdk::{json, Error, Result, Value};
use serde_json::Map;

fn family(model: &str) -> &'static str {
    let model = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .trim()
        .to_ascii_lowercase();
    if ["sn-deepseek-", "deepseek-", "sn-doubao-", "doubao-"]
        .iter()
        .any(|prefix| model.starts_with(prefix))
    {
        "thinking"
    } else if model.starts_with("sn-glm-5-3") || model.starts_with("sn-kimi-k3") {
        "low_high"
    } else if model.starts_with("sn-sensenova-") || model.starts_with("raccoon-") {
        "effort"
    } else {
        ""
    }
}
pub fn supported(model: &str) -> bool {
    !family(model).is_empty()
}
fn rank(level: &str) -> Option<usize> {
    match level.trim().to_ascii_lowercase().as_str() {
        "minimal" => Some(0),
        "low" => Some(1),
        "medium" => Some(2),
        "high" => Some(3),
        "xhigh" => Some(4),
        "max" => Some(5),
        _ => None,
    }
}
fn off(level: &str) -> bool {
    matches!(level.trim().to_ascii_lowercase().as_str(), "off" | "none")
}
fn explicit(body: &Value) -> bool {
    [
        "reasoning_effort",
        "reasoningEffort",
        "reasoning",
        "thinking",
        "enable_thinking",
    ]
    .iter()
    .any(|key| body.get(*key).is_some_and(|value| !value.is_null()))
        || ["thinking", "reasoning_effort", "enable_thinking"]
            .iter()
            .any(|key| {
                body["extra_body"]
                    .get(*key)
                    .is_some_and(|value| !value.is_null())
            })
}
pub fn patch(level: &str, model: &str, body: &Value) -> Value {
    let skip = |reason: &str| json!({ "action": "skip", "reason": reason });
    if explicit(body) {
        return skip("客户端已指定思考参数，保留原值");
    }
    let Some(rank) = rank(level) else {
        return skip("关闭思考或未知档位不注入绑定");
    };
    let (field, value) = match family(model) {
        "thinking" => {
            let mut extra = body["extra_body"].as_object().cloned().unwrap_or_default();
            extra.insert("thinking".into(), json!({ "type": "enabled" }));
            ("extra_body", Value::Object(extra))
        }
        "low_high" => (
            "reasoning_effort",
            json!(if rank <= 1 { "low" } else { "high" }),
        ),
        "effort" => (
            "reasoning_effort",
            json!(match rank {
                0 | 1 => "low",
                2 => "medium",
                _ => "high",
            }),
        ),
        _ => return skip("该模型没有已确认的思考参数"),
    };
    json!({ "action": "set", "field": field, "value": value })
}
pub fn normalize(body: &Value, model: &str) -> Result<Value> {
    let mut body = body.clone();
    let fields = body
        .as_object_mut()
        .ok_or_else(|| Error::http(400, "小浣熊请求体必须是JSON对象"))?;
    if !fields.contains_key("reasoning_effort") {
        if let Some(effort) = fields.remove("reasoningEffort") {
            fields.insert("reasoning_effort".into(), effort);
        }
    }
    match family(model) {
        "thinking" => thinking(fields)?,
        "low_high" => {
            if fields
                .get("thinking")
                .is_some_and(|value| value["type"] == "disabled")
            {
                fields.remove("thinking");
                fields.insert("reasoning_effort".into(), json!("low"));
            } else if let Some(effort) = fields.get("reasoning_effort").and_then(Value::as_str) {
                let mapped = if off(effort) || rank(effort).is_some_and(|rank| rank <= 1) {
                    Some("low")
                } else if rank(effort).is_some() {
                    Some("high")
                } else {
                    None
                };
                if let Some(mapped) = mapped {
                    fields.insert("reasoning_effort".into(), json!(mapped));
                }
            }
        }
        _ => {}
    }
    Ok(body)
}
fn thinking(fields: &mut Map<String, Value>) -> Result<()> {
    if fields
        .get("extra_body")
        .is_some_and(|value| !value.is_null() && !value.is_object())
    {
        return Err(Error::http(400, "小浣熊extra_body必须是JSON对象"));
    }
    let nested = fields
        .get("extra_body")
        .and_then(|extra| extra.get("thinking"))
        .filter(|value| !value.is_null())
        .cloned();
    let direct = fields
        .get("thinking")
        .filter(|value| !value.is_null())
        .cloned();
    let off = fields
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .is_some_and(off);
    if let Some(thinking) = nested
        .or(direct)
        .or_else(|| off.then(|| json!({ "type": "disabled" })))
    {
        if !fields.get("extra_body").is_some_and(Value::is_object) {
            fields.insert("extra_body".into(), json!({}));
        }
        if let Some(extra) = fields.get_mut("extra_body").and_then(Value::as_object_mut) {
            extra.insert("thinking".into(), thinking);
        }
        fields.remove("thinking");
        if off {
            fields.remove("reasoning_effort");
        }
    }
    Ok(())
}
pub fn outbound(body: &Value) -> Value {
    body["reasoning_effort"]
        .as_str()
        .map(|value| json!(value))
        .unwrap_or_else(|| {
            match body
                .pointer("/extra_body/thinking/type")
                .or_else(|| body.pointer("/thinking/type"))
                .and_then(Value::as_str)
            {
                Some("enabled") => json!("high"),
                Some("disabled") => json!("off"),
                _ => Value::Null,
            }
        })
}
