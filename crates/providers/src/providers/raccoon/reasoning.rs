//! 档位字段来自 1.1.5 Box-Agent 的 `_apply_thinking_params`；不覆盖客户端选择。

use agent2api_provider_protocol as proto;
use serde_json::{json, Map, Value};

fn model_name(model: &str) -> String {
    model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .trim()
        .to_ascii_lowercase()
}

fn family(model: &str) -> &'static str {
    let model = model_name(model);
    if model.starts_with("sn-deepseek-")
        || model.starts_with("deepseek-")
        || model.starts_with("sn-doubao-")
        || model.starts_with("doubao-")
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

pub(super) fn supported(model: &str) -> bool {
    !family(model).is_empty()
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
        || body.get("extra_body").is_some_and(|extra| {
            ["thinking", "reasoning_effort", "enable_thinking"]
                .iter()
                .any(|key| extra.get(*key).is_some_and(|value| !value.is_null()))
        })
}

pub(super) fn patch(level: &str, model: &str, body: &Value) -> proto::ReasoningPatch {
    let skip = |reason: &str| proto::ReasoningPatch::Skip {
        reason: reason.to_string(),
    };
    if explicit(body) {
        return skip("客户端已指定思考参数，保留原值");
    }
    let Some(rank) = crate::reasoning::effort_rank(level) else {
        return skip("关闭思考或未知档位不注入绑定");
    };
    let (field, value) = match family(model) {
        "thinking" => {
            let mut extra = body
                .get("extra_body")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            extra.insert("thinking".to_string(), json!({ "type": "enabled" }));
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
        _ => return skip("该小浣熊模型没有已确认的思考参数"),
    };
    proto::ReasoningPatch::Set {
        field: field.to_string(),
        value,
    }
}

pub(super) fn normalize(body: &Value, model: &str) -> Result<Value, String> {
    let mut body = body.clone();
    let Some(fields) = body.as_object_mut() else {
        return Err(
            crate::errors::GatewayError::with_status(400, "小浣熊请求体必须是 JSON 对象")
                .into_message(),
        );
    };
    if !fields.contains_key("reasoning_effort") {
        if let Some(effort) = fields.remove("reasoningEffort") {
            fields.insert("reasoning_effort".to_string(), effort);
        }
    }
    match family(model) {
        "thinking" => normalize_thinking(fields)?,
        "low_high" => {
            let disabled = fields
                .get("thinking")
                .and_then(|value| value.get("type"))
                .and_then(Value::as_str)
                == Some("disabled");
            if disabled {
                fields.remove("thinking");
                fields.insert("reasoning_effort".to_string(), json!("low"));
            } else if let Some(effort) = fields.get("reasoning_effort").and_then(Value::as_str) {
                let mapped = if crate::reasoning::is_thinking_off(effort)
                    || crate::reasoning::effort_rank(effort).is_some_and(|rank| rank <= 1)
                {
                    Some("low")
                } else if crate::reasoning::effort_rank(effort).is_some() {
                    Some("high")
                } else {
                    None
                };
                if let Some(mapped) = mapped {
                    fields.insert("reasoning_effort".to_string(), json!(mapped));
                }
            }
        }
        _ => {}
    }
    Ok(body)
}

fn normalize_thinking(fields: &mut Map<String, Value>) -> Result<(), String> {
    if fields
        .get("extra_body")
        .is_some_and(|value| !value.is_null() && !value.is_object())
    {
        return Err(crate::errors::GatewayError::with_status(
            400,
            "小浣熊 extra_body 必须是 JSON 对象",
        )
        .into_message());
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
        .is_some_and(crate::reasoning::is_thinking_off);
    let thinking = nested
        .or(direct)
        .or_else(|| off.then(|| json!({ "type": "disabled" })));
    if let Some(thinking) = thinking {
        if !fields.get("extra_body").is_some_and(Value::is_object) {
            fields.insert("extra_body".to_string(), json!({}));
        }
        if let Some(extra) = fields.get_mut("extra_body").and_then(Value::as_object_mut) {
            extra.insert("thinking".to_string(), thinking);
        }
        fields.remove("thinking");
        if off {
            fields.remove("reasoning_effort");
        }
    }
    Ok(())
}

pub(super) fn outbound(body: &Value) -> Option<String> {
    body.get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            match body
                .pointer("/extra_body/thinking/type")
                .or_else(|| body.pointer("/thinking/type"))
                .and_then(Value::as_str)
            {
                Some("enabled") => Some("high".to_string()),
                Some("disabled") => Some("off".to_string()),
                _ => None,
            }
        })
}
